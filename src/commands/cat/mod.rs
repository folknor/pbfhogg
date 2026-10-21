//! Concatenate PBF files with optional type filtering. Equivalent to `osmium cat`.

pub mod dedupe;

use std::path::Path;

use super::{
    HeaderOverrides, build_output_header, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, require_indexdata,
};
use crate::blob::{BlobKind, decode_blob_to_headerblock, decompress_blob_data_into};
use crate::blob_meta::{scan_block_ids, scan_block_tags};
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
    pub fn print_summary(&self) {
        if self.blobs_decoded > 0 {
            eprintln!(
                "Decoded {} blobs, wrote {} elements",
                self.blobs_decoded, self.elements_written,
            );
        } else {
            eprintln!("{} blobs passed through", self.blobs_passthrough);
        }
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
) -> Result<CatStats> {
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
            require_indexdata(file, direct_io, force, reason)?;
        }
    }

    crate::debug::emit_marker("CAT_SCAN_START");
    let result = match (type_filter, clean.any()) {
        (None, false) => cat_passthrough(files, output, compression, direct_io, overrides),
        (None, true) => cat_filtered(
            files,
            output,
            TypeFilter::all(),
            clean,
            compression,
            direct_io,
            overrides,
        ),
        (Some(filter), false) => {
            cat_type_passthrough(files, output, filter, compression, direct_io, overrides)
        }
        (Some(filter), true) => cat_filtered(
            files,
            output,
            filter,
            clean,
            compression,
            direct_io,
            overrides,
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
fn read_header(file: &Path, direct_io: bool) -> Result<crate::HeaderBlock> {
    let mut reader = FileReader::open(file, direct_io)?;
    let mut file_offset: u64 = 0;
    while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset)? {
        if frame.blob_type == BlobKind::OsmHeader {
            return Ok(decode_blob_to_headerblock(frame.blob_bytes())?);
        }
    }
    Err("no OSMHeader blob found in input file".into())
}

/// Build the output header bytes for a `cat` run over `files`.
///
/// Single input: the input header, minus anything `build_output_header` drops.
///
/// Multiple inputs: every input's header is read and reconciled, because a
/// concatenation's metadata has to describe the concatenation. Previously all
/// three cat paths built the output header from `files[0]` alone, so the
/// output advertised the first input's bounding box over data drawn from all
/// of them - a file whose declared extent does not cover its own contents,
/// which is worse than declaring none - and carried replication fields from
/// whichever file happened to be first.
///
/// The reconciliation rules, chosen so the header can only ever understate:
///
/// - **bbox**: the union, and only when *every* input declares one. An input
///   without a bbox has unknown extent, so the union is unknown too and the
///   field is omitted rather than guessed.
/// - **replication timestamp / sequence / base URL**: kept per field, only
///   when every input agrees on that field's value. Two inputs from different
///   replication states have no single correct answer, and dropping the field
///   is more honest than picking one.
/// - **`HistoricalInformation`**: the OR. It is a required feature declaring
///   that history metadata *may* be present, so one input carrying history
///   makes it true of the output.
///
/// osmium's `cat` writes a completely blank header for multiple inputs
/// (`command_cat.cpp`), which is the same honesty rule applied bluntly. This
/// keeps whatever the inputs actually agree on, and is documented as a
/// deliberate deviation.
fn cat_output_header_bytes(
    files: &[&Path],
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    if let [single] = files {
        let header = read_header(single, direct_io)?;
        super::warn_locations_on_ways_loss(&header);
        return build_output_header(&header, true, overrides, |hb| hb);
    }

    let headers: Vec<crate::HeaderBlock> = files
        .iter()
        .map(|f| read_header(f, direct_io))
        .collect::<Result<Vec<_>>>()?;
    for header in &headers {
        super::warn_locations_on_ways_loss(header);
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
        hb = hb.bbox(
            fold(f64::min, |b| b.left),
            fold(f64::min, |b| b.bottom),
            fold(f64::max, |b| b.right),
            fold(f64::max, |b| b.top),
        );
    }

    if let Some(ts) = agreed(&headers, crate::HeaderBlock::osmosis_replication_timestamp) {
        hb = hb.replication_timestamp(ts);
    }
    if let Some(seq) = agreed(
        &headers,
        crate::HeaderBlock::osmosis_replication_sequence_number,
    ) {
        hb = hb.replication_sequence_number(seq);
    }
    // Borrowed from `headers`, which outlives `hb` within this function.
    let urls: Vec<Option<&str>> = headers
        .iter()
        .map(crate::HeaderBlock::osmosis_replication_base_url)
        .collect();
    if let Some(first) = urls[0]
        && urls.iter().all(|u| *u == urls[0])
    {
        hb = hb.replication_base_url(first);
    }

    if headers
        .iter()
        .any(crate::HeaderBlock::has_historical_information)
    {
        hb = hb.historical();
    }

    Ok(overrides.apply(hb).build()?)
}

/// The value every header agrees on for one optional field, or `None` when
/// any header lacks it or they disagree.
fn agreed<T: PartialEq + Copy>(
    headers: &[crate::HeaderBlock],
    get: impl Fn(&crate::HeaderBlock) -> Option<T>,
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
) -> Result<CatStats> {
    let header_bytes = cat_output_header_bytes(files, direct_io, overrides)?;

    let mut writer =
        super::writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;
    let mut blobs: u64 = 0;
    let mut decompress_buf: Vec<u8> = Vec::new();

    for file in files {
        let mut reader = FileReader::open(file, direct_io)?;
        let mut file_offset: u64 = 0;

        while let Some(mut frame) = read_raw_frame(&mut reader, &mut file_offset)? {
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
                        let index = match scan_block_ids(&decompress_buf) {
                            Some(idx) => idx,
                            None => {
                                // Unrecognized block - pass through without indexdata.
                                writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
                                decompress_buf.clear();
                                blobs += 1;
                                continue;
                            }
                        };
                        let tagdata = scan_block_tags(&decompress_buf);
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
) -> Result<CatStats> {
    let blob_filter = BlobFilter::new(tf.nodes, tf.ways, tf.relations);

    let header_bytes = cat_output_header_bytes(files, direct_io, overrides)?;

    let mut writer =
        super::writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;
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

        while let Some(mut frame) = read_raw_frame(&mut reader, &mut file_offset)? {
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
/// shape that hit the documented cross-thread `PrimitiveBlock` retention
/// pattern (`src/read/pipeline.rs:66-89`, ~25 GB at planet scale; OOM at
/// 28.9 GB peak measured 2026-04-26 overnight). The pipelined reader allocated
/// blocks on its decode pool and dropped them on the consumer thread; the
/// batch-based mitigation listed in pipeline.rs only reduced the cross-thread
/// window without eliminating it. `parallel_classify_phase` confines each
/// blob's allocation and drop to a single worker thread.
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
) -> Result<CatStats> {
    // Cap glibc arenas to prevent cross-thread alloc/free fragmentation
    // in the per-blob worker pool. Same precedent as check --refs / verify_ids.
    // The workers do BlockBuilder re-encode (allocation-heavy) but each blob's
    // alloc/free cycle is confined to a single worker thread, so the pattern
    // that regresses time-filter (cross-blob scratch reuse defeated by arena
    // capping) doesn't apply.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    // -----------------------------------------------------------------------
    // Output header: reconciled across every input (see `cat_output_header_bytes`)
    // -----------------------------------------------------------------------
    let header_bytes = cat_output_header_bytes(files, direct_io, overrides)?;
    let mut writer =
        super::writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;

    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;

    for file in files {
        let (node_schedule, way_schedule, rel_schedule, shared_file, _max_ids) =
            crate::scan::classify::build_classify_schedules_split(file)?;

        if tf.nodes {
            crate::debug::emit_marker("CAT_NODES_START");
            let (b, e) = run_kind_phase(
                &shared_file,
                &node_schedule,
                crate::blob_meta::ElemKind::Node,
                clean,
                compression,
                &mut writer,
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
    let mut reorder: ReorderBuffer<PhaseResult> = ReorderBuffer::with_capacity(64);
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
    )?;

    if let Some(e) = write_error {
        return Err(e);
    }
    if let Some(e) = classify_error {
        return Err(e.into());
    }

    Ok((total_blobs, total_elements))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
