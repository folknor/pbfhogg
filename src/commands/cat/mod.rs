//! Concatenate PBF files with optional type filtering. Equivalent to `osmium cat`.

pub mod dedupe;

use std::path::Path;

use super::{
    build_output_header, require_indexdata,
    writer_from_header, HeaderOverrides,
    ensure_node_capacity_local, ensure_way_capacity_local, ensure_relation_capacity_local,
};
use crate::owned::{dense_node_metadata, element_metadata, TypeFilter};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::blob::{decode_blob_to_headerblock, decompress_blob_data_into, BlobKind};
use crate::blob_meta::{scan_block_ids, scan_block_tags};
use crate::file_reader::FileReader;
use crate::writer::{reframe_raw_with_index, Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, PrimitiveBlock};

use crate::writer::frame_blob_pipelined;
use super::{flush_local, Result};
use crate::read::raw_frame::read_raw_frame;

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
/// If `type_filter` is set (comma-separated: "node", "way", "relation"),
/// only elements of matching types are included (requires full decode).
/// Without a filter, blobs are passed through as raw bytes (zero decode).
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
pub fn cat(
    files: &[&Path],
    output: &Path,
    type_filter: Option<&str>,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<CatStats> {
    if type_filter.is_some() {
        for file in files {
            require_indexdata(file, direct_io, force,
                "input PBF has no blob-level indexdata. Without indexdata, the type \
                 filter is a no-op - all blobs are decompressed (significantly slower).")?;
        }
    }

    crate::debug::emit_marker("CAT_SCAN_START");
    let result = match (type_filter, clean.any()) {
        (None, false) => cat_passthrough(files, output, compression, direct_io, overrides),
        (None, true) => cat_filtered(files, output, "node,way,relation", clean, compression, direct_io, overrides),
        (Some(filter), false) => cat_type_passthrough(files, output, filter, compression, direct_io, overrides),
        (Some(filter), true) => cat_filtered(files, output, filter, clean, compression, direct_io, overrides),
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
// Passthrough path: no type filter, zero decode
// ---------------------------------------------------------------------------

fn cat_passthrough(files: &[&Path], output: &Path, compression: Compression, direct_io: bool, overrides: &HeaderOverrides) -> Result<CatStats> {
    let single_file = files.len() == 1;

    let header_bytes = {
        let mut reader = FileReader::open(files[0], direct_io)?;
        let mut file_offset: u64 = 0;
        let mut hdr_bytes = None;
        while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset)? {
            if frame.blob_type == BlobKind::OsmHeader {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                super::warn_locations_on_ways_loss(&header);
                hdr_bytes = Some(build_output_header(&header, single_file, overrides, |hb| hb)?);
                break;
            }
        }
        hdr_bytes.ok_or("no OSMHeader blob found in first input file")?
    };

    let mut writer = super::writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;
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
                        let tagdata_bytes = tagdata.as_ref().map(crate::blob_meta::TagIndex::serialize);
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
fn cat_type_passthrough(files: &[&Path], output: &Path, filter: &str, compression: Compression, direct_io: bool, overrides: &HeaderOverrides) -> Result<CatStats> {
    let tf = TypeFilter::parse(filter);
    let single_file = files.len() == 1;
    let blob_filter = BlobFilter::new(tf.nodes, tf.ways, tf.relations);

    let header_bytes = {
        let mut reader = FileReader::open(files[0], direct_io)?;
        let mut file_offset: u64 = 0;
        let mut hdr_bytes = None;
        while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset)? {
            if frame.blob_type == BlobKind::OsmHeader {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                super::warn_locations_on_ways_loss(&header);
                hdr_bytes = Some(build_output_header(&header, single_file, overrides, |hb| hb)?);
                break;
            }
        }
        hdr_bytes.ok_or("no OSMHeader blob found in first input file")?
    };

    let mut writer = super::writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;
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
                        let block = PrimitiveBlock::new_with_scratch(
                            std::mem::take(&mut decompress_buf).into(),
                            &mut st_scratch, &mut gr_scratch,
                        )?;
                        output_blocks.clear();
                        let count = process_block(&block, &mut bb, &mut output_blocks, &tf, &clean, &mut refs_buf)?;
                        flush_local(&mut bb, &mut output_blocks)
                            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                        for (block_bytes, index, tagdata) in output_blocks.drain(..) {
                            writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
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
fn cat_filtered(files: &[&Path], output: &Path, filter: &str, clean: &CleanAttrs, compression: Compression, direct_io: bool, overrides: &HeaderOverrides) -> Result<CatStats> {
    let tf = TypeFilter::parse(filter);
    let single_file = files.len() == 1;

    // Cap glibc arenas to prevent cross-thread alloc/free fragmentation
    // in the per-blob worker pool. Same precedent as check --refs / verify_ids.
    // The workers do BlockBuilder re-encode (allocation-heavy) but each blob's
    // alloc/free cycle is confined to a single worker thread, so the pattern
    // that regresses time-filter (cross-blob scratch reuse defeated by arena
    // capping) doesn't apply.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    // -----------------------------------------------------------------------
    // Read header from first file
    // -----------------------------------------------------------------------
    let first_reader = ElementReader::open(files[0], direct_io)?;
    super::warn_locations_on_ways_loss(first_reader.header());
    let header = first_reader.header().clone();
    let mut writer = writer_from_header(output, compression, &header, single_file, overrides, |hb| hb, direct_io, false)?;
    drop(first_reader);

    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;

    for file in files {
        let (node_schedule, way_schedule, rel_schedule, shared_file) =
            crate::scan::classify::build_classify_schedules_split(file)?;

        if tf.nodes {
            crate::debug::emit_marker("CAT_NODES_START");
            let (b, e) = run_kind_phase(&shared_file, &node_schedule, KIND_NODE, clean, compression, &mut writer)?;
            blobs_decoded += b;
            elements += e;
            crate::debug::emit_marker("CAT_NODES_END");
        }
        if tf.ways {
            crate::debug::emit_marker("CAT_WAYS_START");
            let (b, e) = run_kind_phase(&shared_file, &way_schedule, KIND_WAY, clean, compression, &mut writer)?;
            blobs_decoded += b;
            elements += e;
            crate::debug::emit_marker("CAT_WAYS_END");
        }
        if tf.relations {
            crate::debug::emit_marker("CAT_RELATIONS_START");
            let (b, e) = run_kind_phase(&shared_file, &rel_schedule, KIND_RELATION, clean, compression, &mut writer)?;
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

const KIND_NODE: u8 = 0;
const KIND_WAY: u8 = 1;
const KIND_RELATION: u8 = 2;

/// Run one per-kind phase: pread workers decode + clean + frame each blob
/// of the given kind; main thread writes framed bytes in seq order via
/// `write_raw_owned`.
fn run_kind_phase(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    kind: u8,
    clean: &CleanAttrs,
    compression: Compression,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
) -> Result<(u64, u64)> {
    if schedule.is_empty() {
        return Ok((0, 0));
    }

    let kind_filter = TypeFilter {
        nodes: kind == KIND_NODE,
        ways: kind == KIND_WAY,
        relations: kind == KIND_RELATION,
    };

    type PhaseResult = std::result::Result<(Vec<Vec<u8>>, u64), String>;
    let mut per_blob: Vec<Option<PhaseResult>> = (0..schedule.len()).map(|_| None).collect();

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
            let count = process_block(block, &mut bb, &mut output, &kind_filter, clean, &mut refs_buf)?;
            flush_local(&mut bb, &mut output)?;

            let mut framed: Vec<Vec<u8>> = Vec::with_capacity(output.len());
            for (block_bytes, index, tagdata) in output {
                let indexdata = index.serialize();
                let blob = frame_blob_pipelined(
                    &block_bytes,
                    &compression,
                    Some(indexdata.as_slice()),
                    tagdata.as_deref(),
                )
                .map_err(|e| e.to_string())?;
                framed.push(blob.into_vec());
            }
            Ok((framed, count))
        },
        |seq, r| {
            per_blob[seq] = Some(r);
        },
    )?;

    let mut total_blobs: u64 = 0;
    let mut total_elements: u64 = 0;
    for slot in per_blob {
        let r = slot.expect("parallel_classify_phase must deliver every blob");
        let (framed, count) = r.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        total_elements += count;
        for blob in framed {
            writer.write_raw_owned(blob)?;
            total_blobs += 1;
        }
    }

    Ok((total_blobs, total_elements))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
