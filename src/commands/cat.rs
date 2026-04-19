//! Concatenate PBF files with optional type filtering. Equivalent to `osmium cat`.

use std::path::Path;

use rayon::prelude::*;

use super::{
    build_output_header, dense_node_metadata, element_metadata, require_indexdata,
    for_each_primitive_block_batch_budgeted, writer_from_header, HeaderOverrides, TypeFilter,
    ensure_node_capacity_local, ensure_way_capacity_local, ensure_relation_capacity_local,
    DECODE_BATCH_BYTE_BUDGET,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::blob::{decode_blob_to_headerblock, decompress_blob_data_into, BlobKind};
use crate::blob_meta::{scan_block_ids, scan_block_tags};
use crate::file_reader::FileReader;
use crate::writer::{reframe_raw_with_index, Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, PrimitiveBlock};

use crate::writer::frame_blob_pipelined;
use super::{flush_local, read_raw_frame, Result, BATCH_SIZE};

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

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn cat_filtered(files: &[&Path], output: &Path, filter: &str, clean: &CleanAttrs, compression: Compression, direct_io: bool, overrides: &HeaderOverrides) -> Result<CatStats> {
    let tf = TypeFilter::parse(filter);

    let single_file = files.len() == 1;
    let blob_filter = BlobFilter::new(tf.nodes, tf.ways, tf.relations);

    // -----------------------------------------------------------------------
    // Read header from first file
    // -----------------------------------------------------------------------
    let first_reader = ElementReader::open(files[0], direct_io)?;
    super::warn_locations_on_ways_loss(first_reader.header());
    let header = first_reader.header().clone();
    let mut writer = writer_from_header(output, compression, &header, single_file, overrides, |hb| hb, direct_io, false)?;
    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;

    // -----------------------------------------------------------------------
    // Process each input file
    // -----------------------------------------------------------------------
    let mut batch_count: u64 = 0;
    let mut max_batch_blocks: usize = 0;
    let mut max_batch_bytes: usize = 0;
    let mut total_byte_limited: u64 = 0;
    for file in files {
        let reader = ElementReader::open(file, direct_io)?;
        let blocks_iter = reader.with_blob_filter(blob_filter.clone()).into_blocks_pipelined();
        for_each_primitive_block_batch_budgeted(blocks_iter, BATCH_SIZE, Some(DECODE_BATCH_BYTE_BUDGET), &mut |batch| {
            let batch_bytes: usize = batch.iter().map(PrimitiveBlock::decompressed_size).sum();
            batch_count += 1;
            if batch.len() > max_batch_blocks {
                max_batch_blocks = batch.len();
            }
            if batch_bytes > max_batch_bytes {
                max_batch_bytes = batch_bytes;
            }
            if batch.len() < BATCH_SIZE {
                total_byte_limited += 1;
            }
            let (batch_blobs, batch_elements) = process_batch(
                batch,
                &mut writer,
                compression,
                &tf,
                clean,
            )?;
            blobs_decoded += batch_blobs;
            elements += batch_elements;
            Ok(())
        })?;
    }
    eprintln!("[cat] batches: {batch_count}, max_blocks/batch: {max_batch_blocks}, max_bytes/batch: {:.1} MiB, byte-limited: {total_byte_limited}",
        max_batch_bytes as f64 / (1024.0 * 1024.0));

    writer.flush()?;

    Ok(CatStats {
        blobs_passthrough: 0,
        blobs_decoded,
        elements_written: elements,
    })
}

/// Process a batch of `PrimitiveBlock`s in parallel via rayon.
///
/// Each rayon worker thread decodes, serializes, compresses, and frames blobs
/// in a single parallel pass. The sequential phase writes fully framed blobs
/// via `write_raw_owned`, which has bounded backpressure through the writer
/// thread's `sync_channel`. This avoids the unbounded rayon task queue that
/// caused OOM on planet-scale files.
///
/// Returns `(blobs_decoded, elements_written)`.
fn process_batch(
    batch: &[crate::PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    compression: Compression,
    tf: &TypeFilter,
    clean: &CleanAttrs,
) -> Result<(u64, u64)> {
    // Parallel phase: decode → serialize → compress → frame, all in one pass.
    type BatchResult = std::result::Result<(Vec<Vec<u8>>, u64), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            || (BlockBuilder::new(), Vec::<i64>::new()),
            |(bb, refs_buf), block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let count = process_block(
                    block, bb, &mut output, tf, clean, refs_buf,
                )?;
                flush_local(bb, &mut output)?;

                // Compress and frame each serialized block on this rayon thread.
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
        )
        .collect();

    // Sequential phase: write pre-framed blobs with bounded backpressure.
    let mut total_blobs: u64 = 0;
    let mut total_elements: u64 = 0;
    for result in results {
        let (framed_blobs, count) =
            result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        total_elements += count;
        for blob in framed_blobs {
            writer.write_raw_owned(blob)?;
            total_blobs += 1;
        }
    }

    Ok((total_blobs, total_elements))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
