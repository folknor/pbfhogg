//! Concatenate PBF files with optional type filtering. Equivalent to `osmium cat`.

pub mod dedupe;

use std::path::Path;

use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, require_indexdata,
};
use crate::blob::{BlobKind, decode_blob_to_headerblock, decompress_blob_data_into};
use crate::blob_meta::scan_block_ids_and_tags;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::owned::{TypeFilter, dense_node_metadata, element_metadata};
use crate::writer::{Compression, PbfWriter, reframe_raw_with_index};
use crate::{BlobFilter, Element, PrimitiveBlock};

use super::{Result, flush_local};
use crate::read::raw_frame::read_raw_frame;
use crate::writer::frame_blob_pipelined;

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
pub struct CatStats {
    pub blobs_passthrough: u64,
    pub blobs_decoded: u64,
    pub elements_written: u64,
}

impl CatStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        if self.blobs_decoded > 0 {
            writeln!(
                w,
                "Decoded {} blobs, wrote {} elements",
                self.blobs_decoded, self.elements_written,
            )?;
        } else {
            writeln!(w, "{} blobs passed through", self.blobs_passthrough)?;
        }

        Ok(())
    }
}

/// Concatenate one or more PBF files into a single output.
///
/// If `type_filter` is set, only elements of matching types are included
/// (requires full decode). Without a filter, blobs are passed through as
/// raw bytes (zero decode).
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
pub fn cat(
    files: &[&Path],
    output: &Path,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    if files.is_empty() {
        return Err("no input files specified".into());
    }

    // The indexdata requirement belongs to `cat_filtered`, not to `--type`.
    //
    // It used to be spelled `type_filter.is_some()`, which let `--clean` alone
    // through: that combination also routes to `cat_filtered`, and
    // `build_classify_schedules_split` replicates every *unindexed* blob into
    // all three per-kind schedules, so each blob is read and decompressed three
    // times. The output is correct, which is why nobody noticed - it is three
    // times the work with no warning at all.
    if type_filter.is_some() || clean.any() {
        let reason = if type_filter.is_some() {
            "input PBF has no blob-level indexdata. Without indexdata, the type \
             filter is a no-op - all blobs are decompressed (significantly slower)."
        } else {
            "input PBF has no blob-level indexdata. Without indexdata, --clean \
             decompresses every blob three times - once per element kind - \
             because an unindexed blob has to be scheduled as all three."
        };
        for file in files {
            require_indexdata(file, direct_io, force, reason, host)?;
        }
    }

    crate::debug::emit_marker("CAT_SCAN_START");
    let result = match (type_filter, clean.any()) {
        (None, false) => cat_passthrough(files, output, compression, direct_io, overrides, host),
        (None, true) => cat_filtered(
            files,
            output,
            TypeFilter::all(),
            clean,
            compression,
            direct_io,
            overrides,
            host,
        ),
        (Some(filter), false) => cat_type_passthrough(
            files,
            output,
            filter,
            compression,
            direct_io,
            overrides,
            host,
        ),
        (Some(filter), true) => cat_filtered(
            files,
            output,
            filter,
            clean,
            compression,
            direct_io,
            overrides,
            host,
        ),
    }?;
    crate::debug::emit_marker("CAT_SCAN_END");
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("cat_blobs_passthrough", result.blobs_passthrough as i64);
        crate::debug::emit_counter("cat_blobs_decoded", result.blobs_decoded as i64);
        crate::debug::emit_counter("cat_elements_written", result.elements_written as i64);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Output header
// ---------------------------------------------------------------------------

/// Read one file's `OSMHeader` block.
fn read_header(
    file: &Path,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<crate::HeaderBlock> {
    let mut reader = FileReader::open(file, direct_io)?;
    let mut file_offset: u64 = 0;
    while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset, &host.diagnostics)? {
        if frame.blob_type == BlobKind::OsmHeader {
            return Ok(decode_blob_to_headerblock(frame.blob_bytes())?);
        }
    }
    Err("no OSMHeader blob found in input file".into())
}

/// Build the output header bytes for a plain `cat` run over `files` (every
/// path except `--dedupe`, which collects its headers during its own index
/// pass and calls [`output_header_bytes`] directly).
///
/// A concatenation of one sorted input is still sorted; a concatenation of
/// several is not, however each of them is ordered, so the sort stamp
/// survives only the single-input case.
fn cat_output_header_bytes(
    files: &[&Path],
    direct_io: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<Vec<u8>> {
    let headers: Vec<crate::HeaderBlock> = files
        .iter()
        .map(|f| read_header(f, direct_io, host))
        .collect::<Result<Vec<_>>>()?;
    let sorted = matches!(headers.as_slice(), [only] if only.is_sorted());
    output_header_bytes(&headers, sorted, overrides, host)
}

/// Build the output header bytes for any `cat` path from the headers of
/// *every* input, one per input.
///
/// This takes the whole slice rather than one header on purpose. Every cat
/// path used to build its output header from the first input alone - the
/// three plain paths from `files[0]`, and `--dedupe` from whichever input its
/// index pass met first - so a multi-input output advertised one input's
/// bounding box over data drawn from all of them (a declared extent that does
/// not cover the file's own contents, which is worse than declaring none) and
/// carried replication fields from whichever file happened to be listed
/// first. The fix was once applied per call site and missed one; a single
/// constructor fed every input's header is what keeps the paths from
/// drifting apart again.
///
/// `sorted` is the caller's decision, because only the caller knows whether
/// its output order is established: plain `cat` keeps the stamp for a single
/// sorted input, `--dedupe` always sets it (every input was required to be
/// sorted and the merge emits in type-then-ID order).
fn output_header_bytes(
    headers: &[crate::HeaderBlock],
    sorted: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<Vec<u8>> {
    for header in headers {
        super::warn_locations_on_ways_loss(header, host);
    }
    let mut hb = reconciled_header(headers)?;
    if sorted {
        hb = hb.sorted();
    }
    Ok(overrides.apply(hb).build()?)
}

/// Reconcile the headers of every input into one output header builder.
///
/// The rules are chosen so the header can only ever understate:
///
/// - **bbox**: the union, and only when *every* input declares one. An input
///   without a bbox has unknown extent, so the union is unknown too and the
///   field is omitted rather than guessed.
/// - **replication timestamp / sequence / base URL, and `source`**: kept per
///   field, only when every input agrees on that field's value (absent and
///   present-but-empty are different values). Two inputs from different
///   replication states have no single correct answer, and dropping the field
///   is more honest than picking one.
/// - **`HistoricalInformation`**: the OR. It is a required feature declaring
///   that history metadata *may* be present, so one input carrying history
///   makes it true of the output.
///
/// Optional features (`Sort.Type_then_ID`, `LocationsOnWays`, ...) and the
/// writing program are never carried, exactly as
/// [`HeaderBuilder::from_header`](crate::block_builder::HeaderBuilder::from_header)
/// does not carry them. For a single header the result is therefore the same
/// builder `from_header` produces, so one input and many go through one rule.
///
/// osmium's `cat` writes a completely blank header for multiple inputs
/// (`command_cat.cpp`), which is the same honesty rule applied bluntly. This
/// keeps whatever the inputs actually agree on, and is documented as a
/// deliberate deviation.
fn reconciled_header(
    headers: &[crate::HeaderBlock],
) -> Result<crate::block_builder::HeaderBuilder<'_>> {
    if headers.is_empty() {
        // `agreed` indexes [0] and the bbox `all()` below is vacuously true on
        // an empty slice; refuse here, where that precondition is relied on.
        return Err("no input headers to reconcile (no input files specified)".into());
    }

    let mut hb = crate::block_builder::HeaderBuilder::new();

    if headers.iter().all(|h| h.bbox().is_some()) {
        let boxes: Vec<_> = headers
            .iter()
            .filter_map(crate::HeaderBlock::bbox)
            .collect();
        let fold = |pick: fn(f64, f64) -> f64, get: fn(&crate::HeaderBBox) -> f64| {
            boxes.iter().map(get).fold(
                f64::NAN,
                |acc, v| {
                    if acc.is_nan() { v } else { pick(acc, v) }
                },
            )
        };
        // The union of input header boxes is still the inputs' claim, so it
        // round-trips unchecked like a single copied header box.
        hb = hb.input_bbox(
            fold(f64::min, |b| b.left),
            fold(f64::min, |b| b.bottom),
            fold(f64::max, |b| b.right),
            fold(f64::max, |b| b.top),
        );
    }

    if let Some(ts) = agreed(headers, crate::HeaderBlock::osmosis_replication_timestamp) {
        hb = hb.replication_timestamp(ts);
    }
    if let Some(seq) = agreed(
        headers,
        crate::HeaderBlock::osmosis_replication_sequence_number,
    ) {
        hb = hb.replication_sequence_number(seq);
    }
    if let Some(url) = agreed(headers, crate::HeaderBlock::osmosis_replication_base_url) {
        hb = hb.replication_base_url(url);
    }
    if let Some(source) = agreed(headers, crate::HeaderBlock::source) {
        hb = hb.source(source);
    }

    if headers
        .iter()
        .any(crate::HeaderBlock::has_historical_information)
    {
        hb = hb.historical();
    }

    Ok(hb)
}

/// The value every header agrees on for one optional field, or `None` when
/// any header lacks it or they disagree. Values are compared and returned
/// as-is, so `Some(0)` and `Some("")` are real agreed values, distinct from
/// absence. Callers guarantee `headers` is non-empty.
fn agreed<'a, T: PartialEq + Copy>(
    headers: &'a [crate::HeaderBlock],
    get: impl Fn(&'a crate::HeaderBlock) -> Option<T>,
) -> Option<T> {
    let first = get(&headers[0])?;
    headers
        .iter()
        .all(|h| get(h) == Some(first))
        .then_some(first)
}

// ---------------------------------------------------------------------------
// Passthrough path: no type filter, zero decode
// ---------------------------------------------------------------------------

fn cat_passthrough(
    files: &[&Path],
    output: &Path,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let header_bytes = cat_output_header_bytes(files, direct_io, overrides, host)?;

    let mut writer = super::writer_from_header_bytes(
        output,
        compression,
        &header_bytes,
        direct_io,
        false,
        host,
    )?;
    let mut blobs: u64 = 0;
    let mut decompress_buf: Vec<u8> = Vec::new();

    for file in files {
        let mut reader = FileReader::open(file, direct_io)?;
        let mut file_offset: u64 = 0;

        while let Some(mut frame) =
            read_raw_frame(&mut reader, &mut file_offset, &host.diagnostics)?
        {
            match &frame.blob_type {
                BlobKind::OsmHeader => {}
                BlobKind::OsmData => {
                    if frame.index.is_some() {
                        // Already has indexdata - pass through as-is.
                        writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
                    } else {
                        // No indexdata - decompress to scan IDs/tags, reframe with index.
                        let blob_bytes = frame.blob_bytes();
                        decompress_blob_data_into(blob_bytes, &mut decompress_buf)?;
                        // Name the input and the blob's frame offset through the
                        // crate error's own location fields, which print as
                        // "(in PATH, at byte offset N)". `From<io::Error>`
                        // unwraps the scanner's own error from the io carrier,
                        // so its kind survives rather than a stringified copy.
                        let (index, tagdata) =
                            scan_block_ids_and_tags(&decompress_buf).map_err(|source| {
                                crate::error::Error::from(source)
                                    .at(crate::ByteOffset(frame.file_offset))
                                    .in_input(Some(&std::sync::Arc::<Path>::from(*file)), None)
                            })?;
                        let index = match index {
                            Some(idx) => idx,
                            None => {
                                // Unrecognized block - pass through without indexdata.
                                writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
                                decompress_buf.clear();
                                blobs += 1;
                                continue;
                            }
                        };
                        let tagdata_bytes = tagdata
                            .as_ref()
                            .and_then(crate::blob_meta::TagIndex::serialize);
                        let reframed = reframe_raw_with_index(
                            blob_bytes,
                            &index.serialize(),
                            tagdata_bytes.as_deref(),
                        )?;
                        decompress_buf.clear();
                        writer.write_raw_owned(reframed)?;
                    }
                    blobs += 1;
                }
                _ => {}
            }
        }
    }

    writer.flush()?;
    Ok(CatStats {
        blobs_passthrough: blobs,
        blobs_decoded: 0,
        elements_written: 0,
    })
}

// ---------------------------------------------------------------------------
// Type-filtered passthrough: raw frame copy for matching blob types
// ---------------------------------------------------------------------------

/// Type-filtered cat via raw frame passthrough. Matching blobs (by indexdata
/// ElemKind) are written as-is - zero decompression, zero re-encoding.
/// Non-matching blobs are skipped. Blobs without indexdata fall back to full
/// decode + re-encode.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn cat_type_passthrough(
    files: &[&Path],
    output: &Path,
    tf: TypeFilter,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let blob_filter = BlobFilter::new(tf.nodes, tf.ways, tf.relations);

    let header_bytes = cat_output_header_bytes(files, direct_io, overrides, host)?;

    let mut writer = super::writer_from_header_bytes(
        output,
        compression,
        &header_bytes,
        direct_io,
        false,
        host,
    )?;
    let mut blobs_passthrough: u64 = 0;
    let mut blobs_decoded: u64 = 0;
    let mut elements_written: u64 = 0;

    let clean = CleanAttrs::default(); // no-op clean

    // Hoisted outside per-blob loop: reused across iterations.
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut bb = BlockBuilder::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();

    for file in files {
        let mut reader = FileReader::open(file, direct_io)?;
        let mut file_offset: u64 = 0;

        while let Some(mut frame) =
            read_raw_frame(&mut reader, &mut file_offset, &host.diagnostics)?
        {
            match &frame.blob_type {
                BlobKind::OsmHeader => {}
                BlobKind::OsmData => {
                    if let Some(ref idx) = frame.index {
                        if !blob_filter.wants_index(idx) {
                            continue;
                        }
                        writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
                        blobs_passthrough += 1;
                    } else {
                        let blob_bytes = frame.blob_bytes();
                        decompress_buf.clear();
                        decompress_blob_data_into(blob_bytes, &mut decompress_buf)?;
                        // Take the decompressed Vec straight into the block: no
                        // Bytes round-trip, no second whole-buffer copy (the
                        // dropped `new_with_scratch(Bytes)` route paid a `to_vec`).
                        // `decompress_buf` is left empty and reallocated next blob.
                        let block = PrimitiveBlock::from_vec_with_scratch(
                            std::mem::take(&mut decompress_buf),
                            &mut st_scratch,
                            &mut gr_scratch,
                        )?;
                        output_blocks.clear();
                        let count = process_block(
                            &block,
                            &mut bb,
                            &mut output_blocks,
                            &tf,
                            &clean,
                            &mut refs_buf,
                        )?;
                        flush_local(&mut bb, &mut output_blocks)
                            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                        for OwnedBlock {
                            bytes: block_bytes,
                            index,
                            tagdata,
                            way_members,
                        } in output_blocks.drain(..)
                        {
                            writer.write_primitive_block_owned(
                                block_bytes,
                                index,
                                tagdata.as_deref(),
                                way_members.as_deref(),
                            )?;
                        }
                        blobs_decoded += 1;
                        elements_written += count;
                    }
                }
                _ => {}
            }
        }
    }

    writer.flush()?;
    Ok(CatStats {
        blobs_passthrough,
        blobs_decoded,
        elements_written,
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
    output: &mut Vec<OwnedBlock>,
    tf: &TypeFilter,
    clean: &CleanAttrs,
    refs_buf: &mut Vec<i64>,
) -> std::result::Result<u64, String> {
    let (filter_node, filter_way, filter_relation) = (tf.nodes, tf.ways, tf.relations);
    let mut count: u64 = 0;
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) if filter_node => {
                ensure_node_capacity_local(bb, output)?;
                let meta = clean_metadata(dense_node_metadata(dn), clean);
                bb.add_node(
                    dn.id(),
                    dn.decimicro_lat(),
                    dn.decimicro_lon(),
                    dn.tags(),
                    meta.as_ref(),
                );
                count += 1;
            }
            Element::Node(n) if filter_node => {
                ensure_node_capacity_local(bb, output)?;
                let meta = clean_metadata(element_metadata(&n.info()), clean);
                bb.add_node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    n.tags(),
                    meta.as_ref(),
                );
                count += 1;
            }
            Element::Way(w) if filter_way => {
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = clean_metadata(element_metadata(&w.info()), clean);
                bb.add_way(w.id(), w.tags(), refs_buf, meta.as_ref());
                count += 1;
            }
            Element::Relation(r) if filter_relation => {
                ensure_relation_capacity_local(bb, output)?;
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = clean_metadata(element_metadata(&r.info()), clean);
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                count += 1;
            }
            _ => {}
        }
    }

    Ok(count)
}

use super::clean_metadata;

/// Type-filtered cat with full decode + re-encode (used for `--clean` and for
/// type-filtered passes when raw passthrough doesn't apply).
///
/// Uses `parallel_classify_phase` per kind. Each worker pread's a blob from
/// the shared file, decodes it inline (alloc on the worker thread), processes
/// matching elements through a thread-local `BlockBuilder`, frames the output
/// blobs, and returns the framed bytes back to the main thread for sequential
/// writing in seq order.
///
/// Replaces an earlier `into_blocks_pipelined` + `for_each_primitive_block_batch_budgeted`
/// shape that hit the cross-thread `PrimitiveBlock` retention pattern of the
/// pipelined reader at the time (~25 GB at planet scale; OOM at 28.9 GB peak
/// measured 2026-04-26 overnight): blocks were allocated on its decode pool
/// and dropped on the consumer thread, so glibc kept the freed pages on the
/// decode threads' arenas, and batching only narrowed that window without
/// closing it. `parallel_classify_phase` confines each blob's allocation and
/// drop to a single worker thread. The pipelined reader has since moved its
/// string-table entries inline into the pooled decompression buffer (see its
/// "Memory: decoded-block working set" doc in `src/read/pipeline.rs`), so
/// this history no longer describes a live hazard there.
///
/// **Output ordering note.** Both modes are type-sorted: nodes first, then
/// ways, then relations. For inputs that are already type-sorted (the
/// production case), this preserves the existing output structure. For
/// unsorted inputs, the output is re-sorted into type order. For mixed-type
/// blobs (rare in practice; PBFs conventionally have one kind per block),
/// each kind is emitted in its own phase, splitting one input blob into up
/// to three smaller output blobs.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn cat_filtered(
    files: &[&Path],
    output: &Path,
    tf: TypeFilter,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    // Per-blob worker pool; same precedent as check --refs / verify_ids.
    // The workers do BlockBuilder re-encode (allocation-heavy) but each
    // blob's alloc/free cycle is confined to a single worker thread, so
    // the cross-blob scratch reuse that arena capping penalises (measured
    // on the pre-migration time-filter snapshot path) doesn't apply.
    host.process.cross_thread_alloc_pool();

    // -----------------------------------------------------------------------
    // Output header: reconciled across every input (see `cat_output_header_bytes`)
    // -----------------------------------------------------------------------
    let header_bytes = cat_output_header_bytes(files, direct_io, overrides, host)?;
    let mut writer = super::writer_from_header_bytes(
        output,
        compression,
        &header_bytes,
        direct_io,
        false,
        host,
    )?;

    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;

    for file in files {
        // The fifth result sizes IdSets in check callers; filtered cat only
        // needs the schedules.
        let (node_schedule, way_schedule, rel_schedule, shared_file, _) =
            crate::scan::classify::build_classify_schedules_split(file, &host.diagnostics)?;

        if tf.nodes {
            crate::debug::emit_marker("CAT_NODES_START");
            let (b, e) = run_kind_phase(
                &shared_file,
                &node_schedule,
                crate::blob_meta::ElemKind::Node,
                clean,
                compression,
                &mut writer,
                host,
            )?;
            blobs_decoded += b;
            elements += e;
            crate::debug::emit_marker("CAT_NODES_END");
        }
        if tf.ways {
            crate::debug::emit_marker("CAT_WAYS_START");
            let (b, e) = run_kind_phase(
                &shared_file,
                &way_schedule,
                crate::blob_meta::ElemKind::Way,
                clean,
                compression,
                &mut writer,
                host,
            )?;
            blobs_decoded += b;
            elements += e;
            crate::debug::emit_marker("CAT_WAYS_END");
        }
        if tf.relations {
            crate::debug::emit_marker("CAT_RELATIONS_START");
            let (b, e) = run_kind_phase(
                &shared_file,
                &rel_schedule,
                crate::blob_meta::ElemKind::Relation,
                clean,
                compression,
                &mut writer,
                host,
            )?;
            blobs_decoded += b;
            elements += e;
            crate::debug::emit_marker("CAT_RELATIONS_END");
        }
    }

    writer.flush()?;

    Ok(CatStats {
        blobs_passthrough: 0,
        blobs_decoded,
        elements_written: elements,
    })
}

/// Run one per-kind phase: pread workers decode + clean + frame each blob
/// of the given kind; main thread streams framed bytes to the writer in seq
/// order via a `ReorderBuffer`.
///
/// The reorder buffer is essential here. Worker output is large
/// (~1.6 MB framed per planet node blob × 47K node blobs ≈ 75 GB if the
/// whole phase's output were accumulated before writing). Streaming with
/// `pop_ready` after every push caps the in-flight set to roughly the
/// decode-thread count (~24 on a 24-thread host), bounding peak RSS to
/// ~50 MB of framed bytes per phase regardless of input size.
fn run_kind_phase(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    kind: crate::blob_meta::ElemKind,
    clean: &CleanAttrs,
    compression: Compression,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    host: &crate::host::Host,
) -> Result<(u64, u64)> {
    use crate::blob_meta::ElemKind;
    use crate::reorder_buffer::ReorderBuffer;

    if schedule.is_empty() {
        return Ok((0, 0));
    }

    let kind_filter = TypeFilter {
        nodes: kind == ElemKind::Node,
        ways: kind == ElemKind::Way,
        relations: kind == ElemKind::Relation,
    };

    type PhaseResult = std::result::Result<(Vec<Vec<u8>>, u64), String>;
    let mut reorder: ReorderBuffer<PhaseResult> =
        ReorderBuffer::with_capacity(crate::tunables::POOL_REORDER_CAPACITY_WIDE);
    let mut total_blobs: u64 = 0;
    let mut total_elements: u64 = 0;
    let mut write_error: Option<Box<dyn std::error::Error>> = None;
    let mut classify_error: Option<String> = None;

    // BlockBuilder contains `Rc<str>` (string interning) which is not Send,
    // so it can't ride the `S: Send` worker-state slot. Per-blob alloc inside
    // the closure is cheap (BlockBuilder::new is just a few empty Vec/HashMap
    // initialisers; no heap reservation until elements are added).
    crate::scan::classify::parallel_classify_phase(
        shared_file,
        schedule,
        None,
        || (),
        |block, _state| -> PhaseResult {
            let mut bb = BlockBuilder::new();
            let mut refs_buf: Vec<i64> = Vec::new();
            let mut output: Vec<OwnedBlock> = Vec::new();
            let count = process_block(
                block,
                &mut bb,
                &mut output,
                &kind_filter,
                clean,
                &mut refs_buf,
            )?;
            flush_local(&mut bb, &mut output)?;

            let mut framed: Vec<Vec<u8>> = Vec::with_capacity(output.len());
            for OwnedBlock {
                bytes: block_bytes,
                index,
                tagdata,
                way_members,
            } in output
            {
                let indexdata = index.serialize();
                let blob = frame_blob_pipelined(
                    &block_bytes,
                    &compression,
                    Some(indexdata.as_slice()),
                    tagdata.as_deref(),
                    way_members.as_deref(),
                )
                .map_err(|e| e.to_string())?;
                framed.push(blob.into_vec());
            }
            Ok((framed, count))
        },
        |seq, r| {
            // Always queue the result so the next-seq invariant holds; we
            // can't drop a slot mid-phase without breaking the reorder
            // buffer's contiguous-prefix expectation.
            reorder.push(seq, r);
            // Drain everything ready from the front. Once we hit a hole
            // (next seq not yet delivered), stop and wait for the next
            // merge call. Errors are captured into local Option slots and
            // propagated after parallel_classify_phase returns; further
            // ready items are still drained so the buffer doesn't grow.
            while let Some(r) = reorder.pop_ready() {
                match r {
                    Ok((framed, count)) => {
                        if write_error.is_some() {
                            continue;
                        }
                        for blob in framed {
                            if let Err(e) = writer.write_raw_owned(blob) {
                                write_error = Some(e.into());
                                break;
                            }
                            total_blobs += 1;
                        }
                        total_elements += count;
                    }
                    Err(e) => {
                        if classify_error.is_none() {
                            classify_error = Some(e);
                        }
                    }
                }
            }
        },
        &*host.resources,
    )?;

    if let Some(e) = write_error {
        return Err(e);
    }
    if let Some(e) = classify_error {
        return Err(e.into());
    }

    Ok((total_blobs, total_elements))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{HeaderOverrides, reconciled_header};
    use crate::block_builder::HeaderBuilder;

    fn parse(bytes: &[u8]) -> crate::HeaderBlock {
        crate::HeaderBlock::parse_from_bytes(bytes).unwrap()
    }

    /// A header exercising every field `HeaderBuilder::build` emits, with the
    /// values most likely to be normalized away by accident: replication
    /// timestamp and sequence number 0, and an empty base URL.
    fn full_header() -> crate::HeaderBlock {
        parse(
            &HeaderBuilder::new()
                .bbox(1.0, 10.0, 2.0, 20.0)
                .replication_timestamp(0)
                .replication_sequence_number(0)
                .replication_base_url("")
                .source("geofabrik.de")
                .historical()
                .sorted()
                .optional_feature(crate::read::block::Feature::LOCATIONS_ON_WAYS)
                .writing_program("other-tool")
                .build()
                .unwrap(),
        )
    }

    /// One input and many go through the same reconciler, so for a single
    /// header it must produce exactly the bytes the single-input path used to
    /// produce via `build_output_header(.., preserve_sorted = true, ..)`. A
    /// byte comparison, not a field-by-field one, so a field the reconciler
    /// forgets to carry cannot pass.
    #[test]
    fn a_single_header_reconciles_to_the_old_single_input_bytes() {
        let header = full_header();
        for overrides in [
            HeaderOverrides::default(),
            HeaderOverrides {
                generator: Some("gen".to_string()),
                replication_timestamp: Some(5),
                replication_sequence_number: None,
                replication_base_url: Some("https://override.invalid/".to_string()),
            },
        ] {
            let old =
                crate::commands::build_output_header(&header, true, &overrides, |hb| hb).unwrap();
            let new = overrides
                .apply(
                    reconciled_header(std::slice::from_ref(&header))
                        .unwrap()
                        .sorted(),
                )
                .build()
                .unwrap();
            assert_eq!(new, old);
        }
    }

    #[test]
    fn source_survives_only_when_every_input_agrees() {
        let with = |s: Option<&str>| {
            let mut hb = HeaderBuilder::new();
            if let Some(s) = s {
                hb = hb.source(s);
            }
            parse(&hb.build().unwrap())
        };
        let agree = [with(Some("a")), with(Some("a"))];
        let reconciled = parse(&reconciled_header(&agree).unwrap().build().unwrap());
        assert_eq!(reconciled.source(), Some("a"));

        for disagree in [
            [with(Some("a")), with(Some("b"))],
            [with(Some("a")), with(None)],
        ] {
            let reconciled = parse(&reconciled_header(&disagree).unwrap().build().unwrap());
            assert_eq!(reconciled.source(), None, "a disagreed source is dropped");
        }
    }

    /// The reconciler indexes the first header and folds `all()` over the
    /// slice, which is vacuously true when empty. An empty slice must be an
    /// error, not a panic and not a header claiming a bbox of NaNs.
    #[test]
    fn an_empty_header_slice_is_refused() {
        assert!(reconciled_header(&[]).is_err());
    }
}
