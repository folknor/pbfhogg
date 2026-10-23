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
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter, reframe_raw_with_index};
use crate::{BlobFilter, Element, PrimitiveBlock};

use super::output_header::{OutputFacts, OutputHeader, Ways};
use super::{Result, flush_local};
use crate::error::Error;
use crate::read::raw_frame::{frame_location, read_raw_frame};
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
pub fn cat(
    files: &[&Path],
    output: crate::output::Output,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    host.cancel.check()?;
    let result = cat_to(files, &output, type_filter, clean, write, overrides, host)?;
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`cat`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn cat_to(
    files: &[&Path],
    output: &crate::output::Output,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let (direct_io, force) = (write.direct_reads, write.force);
    if files.is_empty() {
        return Err(Error::invalid_argument("no input files specified"));
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
            "input PBF has no blob-level indexdata. Without indexdata, cleaning \
             metadata decompresses every blob three times - once per element kind - \
             because an unindexed blob has to be scheduled as all three."
        };
        for file in files {
            require_indexdata(file, direct_io, force, reason, host)?;
        }
    }

    crate::debug::emit_marker("CAT_SCAN_START");
    let result = match (type_filter, clean.any()) {
        (None, false) => cat_passthrough(files, output, write, overrides, host),
        (None, true) => cat_filtered(
            files,
            output,
            TypeFilter::all(),
            clean,
            write,
            overrides,
            host,
        ),
        (Some(filter), false) => {
            cat_type_passthrough(files, output, filter, write, overrides, host)
        }
        (Some(filter), true) => cat_filtered(files, output, filter, clean, write, overrides, host),
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

/// Read one file's `OSMHeader` block. The header is the first frame; a file
/// that starts with anything else has none (a later one is refused by the
/// frame decoder).
fn read_header(
    file: &Path,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<crate::HeaderBlock> {
    let mut reader = FileReader::open(file, direct_io)?;
    let mut file_offset: u64 = 0;
    match read_raw_frame(&mut reader, &mut file_offset, host, file)? {
        Some(frame) if frame.blob_type == BlobKind::OsmHeader => {
            decode_blob_to_headerblock(frame.blob_bytes())
                .map_err(|e| e.locate(frame_location(file, frame.file_offset)))
        }
        _ => Err(crate::error::new_missing_header(file)),
    }
}

/// Read every input's `OSMHeader`, one per input, in input order.
pub(super) fn read_headers(
    files: &[&Path],
    direct_io: bool,
    host: &crate::host::Host,
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
    files: &[&Path],
    headers: &[crate::HeaderBlock],
    direct_io: bool,
    unindexed: Ways,
    host: &crate::host::Host,
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
        if !super::has_indexdata(file, direct_io, &host.diagnostics)? {
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
    host: &crate::host::Host,
) -> Result<OutputHeader> {
    super::output_header::build(headers, &OutputFacts::carry(ways), overrides, host)
}

// ---------------------------------------------------------------------------
// Passthrough path: no type filter, zero decode
// ---------------------------------------------------------------------------

fn cat_passthrough(
    files: &[&Path],
    output: &crate::output::Output,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let direct_io = write.direct_reads;
    // Every payload is copied byte for byte; an unindexed blob's header is
    // rebuilt to add indexdata, which drops its field 5.
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
    let mut blobs: u64 = 0;
    let mut decompress_buf: Vec<u8> = Vec::new();

    for file in files {
        let mut reader = FileReader::open(file, direct_io)?;
        let mut file_offset: u64 = 0;

        while let Some(mut frame) = read_raw_frame(&mut reader, &mut file_offset, host, file)? {
            match &frame.blob_type {
                BlobKind::OsmHeader => {}
                BlobKind::OsmData => {
                    if frame.index.is_some() {
                        // Already has indexdata - pass through as-is.
                        writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
                    } else {
                        // No indexdata - decompress to scan IDs/tags, reframe with index.
                        let blob_bytes = frame.blob_bytes();
                        // Name the input and the blob's frame offset through
                        // the crate error's own location, which prints as
                        // "(in PATH, at byte offset N)". The walk owns the
                        // provenance; the frame carries its true offset.
                        let locate = |e: Error| e.locate(frame_location(file, frame.file_offset));
                        decompress_blob_data_into(blob_bytes, &mut decompress_buf)
                            .map_err(locate)?;
                        let (index, tagdata) =
                            scan_block_ids_and_tags(&decompress_buf).map_err(locate)?;
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
                        if declared.way_members && index.kind == crate::ElementKind::Way {
                            return Err(locate(
                                declared.broken_by_unindexed_way_blob("adding its indexdata"),
                            ));
                        }
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
/// ElementKind) are written as-is - zero decompression, zero re-encoding.
/// Non-matching blobs are skipped. Blobs without indexdata fall back to full
/// decode + re-encode.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn cat_type_passthrough(
    files: &[&Path],
    output: &crate::output::Output,
    tf: TypeFilter,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let blob_filter = BlobFilter::new(tf.nodes, tf.ways, tf.relations);
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

        while let Some(mut frame) = read_raw_frame(&mut reader, &mut file_offset, host, file)? {
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
                        let locate = |e: Error| e.locate(frame_location(file, frame.file_offset));
                        decompress_blob_data_into(blob_bytes, &mut decompress_buf)
                            .map_err(locate)?;
                        // Take the decompressed Vec straight into the block: no
                        // Bytes round-trip, no second whole-buffer copy (the
                        // dropped `new_with_scratch(Bytes)` route paid a `to_vec`).
                        // `decompress_buf` is left empty and reallocated next blob.
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
                                .any(|b| b.index.kind == crate::ElementKind::Way)
                        {
                            return Err(locate(
                                declared.broken_by_unindexed_way_blob("re-encoding it"),
                            ));
                        }
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
) -> crate::Result<u64> {
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
#[allow(clippy::too_many_lines)]
fn cat_filtered(
    files: &[&Path],
    output: &crate::output::Output,
    tf: TypeFilter,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let (compression, direct_io) = (write.compression, write.direct_reads);
    // Per-blob worker pool; same precedent as check --refs / verify_ids.
    // The workers do BlockBuilder re-encode (allocation-heavy) but each
    // blob's alloc/free cycle is confined to a single worker thread, so
    // the cross-blob scratch reuse that arena capping penalises (measured
    // on the pre-migration time-filter snapshot path) doesn't apply.
    host.process.cross_thread_alloc_pool();

    // -----------------------------------------------------------------------
    // Output header: reconciled across every input; every way re-encoded.
    // -----------------------------------------------------------------------
    let headers = read_headers(files, direct_io, host)?;
    let header = cat_output_header(&headers, Ways::Reencoded, overrides, host)?;
    let mut writer =
        super::writer_from_header_bytes(output, compression, &header.bytes, write.backend, host)?;

    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;

    for file in files {
        // The fifth result sizes IdSets in check callers; filtered cat only
        // needs the schedules.
        let (node_schedule, way_schedule, rel_schedule, shared_file, _) =
            crate::scan::classify::build_classify_schedules_split(file, host)?;

        if tf.nodes {
            crate::debug::emit_marker("CAT_NODES_START");
            let (b, e) = run_kind_phase(
                &shared_file,
                &node_schedule,
                crate::ElementKind::Node,
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
                crate::ElementKind::Way,
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
                crate::ElementKind::Relation,
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
    input: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    kind: crate::ElementKind,
    clean: &CleanAttrs,
    compression: Compression,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    host: &crate::host::Host,
) -> Result<(u64, u64)> {
    use crate::ElementKind;
    use crate::reorder_buffer::ReorderBuffer;

    if schedule.is_empty() {
        return Ok((0, 0));
    }

    let kind_filter = TypeFilter {
        nodes: kind == ElementKind::Node,
        ways: kind == ElementKind::Way,
        relations: kind == ElementKind::Relation,
    };

    type PhaseResult = crate::Result<(Vec<Vec<u8>>, u64)>;
    let mut reorder: ReorderBuffer<(Vec<Vec<u8>>, u64)> =
        ReorderBuffer::with_capacity(crate::tunables::POOL_REORDER_CAPACITY_WIDE);
    let mut total_blobs: u64 = 0;
    let mut total_elements: u64 = 0;
    let mut write_error: Option<Error> = None;

    // BlockBuilder contains `Rc<str>` (string interning) which is not Send,
    // so it can't ride the `S: Send` worker-state slot. Per-blob alloc inside
    // the closure is cheap (BlockBuilder::new is just a few empty Vec/HashMap
    // initialisers; no heap reservation until elements are added). A failed
    // blob ends the pool, located with its input and ordinal.
    let pool_result = crate::scan::classify::parallel_classify_phase(
        input,
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
                )?;
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
            // merge call. A write error is captured into a local slot and
            // propagated after parallel_classify_phase returns; further
            // ready items are still drained so the buffer doesn't grow.
            while let Some((framed, count)) = reorder.pop_ready() {
                if write_error.is_some() {
                    continue;
                }
                for blob in framed {
                    if let Err(e) = writer.write_raw_owned(blob) {
                        write_error = Some(e);
                        break;
                    }
                    total_blobs += 1;
                }
                total_elements += count;
            }
        },
        host,
    );

    // An error the merge recorded came first: the pool's own (a cancellation
    // observed at a later receive included) is the later one.
    if let Some(e) = write_error {
        return Err(e);
    }
    pool_result?;

    Ok((total_blobs, total_elements))
}
