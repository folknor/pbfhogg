//! Concatenate PBF files with optional type filtering. Equivalent to `osmium cat`.

use std::io::{self, Read};
use std::path::Path;

use rayon::prelude::*;

use super::{dense_node_metadata, element_metadata, require_indexdata, TypeFilter};
use crate::block_builder::{BlockBuilder, HeaderBuilder, MemberData, OwnedBlock};
use crate::blob::{decode_blob_to_headerblock, parse_blob_header, BlobKind};
use crate::file_reader::FileReader;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader};

use super::{drain_batch_results, flush_local, Result, BATCH_SIZE};

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
#[hotpath::measure]
pub fn cat(
    files: &[&Path],
    output: &Path,
    type_filter: Option<&str>,
    compression: Compression,
    direct_io: bool,
    force: bool,
) -> Result<CatStats> {
    if type_filter.is_some() {
        if let Some(first) = files.first() {
            require_indexdata(first, direct_io, force,
                "input PBF has no blob-level indexdata. Without indexdata, the type \
                 filter is a no-op — all blobs are decompressed (significantly slower).")?;
        }
    }

    match type_filter {
        None => cat_passthrough(files, output, compression, direct_io),
        Some(filter) => cat_filtered(files, output, filter, compression, direct_io),
    }
}

// ---------------------------------------------------------------------------
// Passthrough path: no type filter, zero decode
// ---------------------------------------------------------------------------

/// Raw blob frame: complete framed bytes for write_raw() passthrough.
struct RawBlobFrame {
    frame_bytes: Vec<u8>,
    blob_type: BlobKind,
    blob_bytes: Vec<u8>,
    /// Byte offset of this frame in the input file (for copy_file_range).
    #[cfg_attr(not(feature = "linux-direct-io"), allow(dead_code))]
    file_offset: u64,
}

/// Read the next raw blob frame. Returns None at EOF.
/// Updates `file_offset` to track position for copy_file_range.
fn read_raw_frame<R: Read>(
    reader: &mut R,
    file_offset: &mut u64,
) -> Result<Option<RawBlobFrame>> {
    let frame_start = *file_offset;

    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes(len_buf) as usize;

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;
    let (blob_type, data_size) = parse_blob_header(&header_bytes)?;

    let mut blob_bytes = vec![0u8; data_size];
    reader.read_exact(&mut blob_bytes)?;

    let frame_len = 4 + header_len + data_size;
    *file_offset += frame_len as u64;
    let mut frame_bytes = Vec::with_capacity(frame_len);
    frame_bytes.extend_from_slice(&len_buf);
    frame_bytes.extend_from_slice(&header_bytes);
    frame_bytes.extend_from_slice(&blob_bytes);

    Ok(Some(RawBlobFrame {
        frame_bytes,
        blob_type,
        blob_bytes,
        file_offset: frame_start,
    }))
}

fn cat_passthrough(files: &[&Path], output: &Path, compression: Compression, direct_io: bool) -> Result<CatStats> {
    let single_file = files.len() == 1;

    let header_bytes = {
        let mut reader = FileReader::open(files[0], direct_io)?;
        let mut file_offset: u64 = 0;
        let mut hdr_bytes = None;
        while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset)? {
            if frame.blob_type == BlobKind::OsmHeader {
                let header = decode_blob_to_headerblock(&frame.blob_bytes)?;
                let mut hb = HeaderBuilder::from_header(&header);
                if single_file && header.is_sorted() {
                    hb = hb.sorted();
                }
                hdr_bytes = Some(hb.build()?);
                break;
            }
        }
        hdr_bytes.ok_or("no OSMHeader blob found in first input file")?
    };

    let mut writer = PbfWriter::to_path(output, compression, &header_bytes)?;
    let mut blobs: u64 = 0;

    for file in files {
        let mut reader = FileReader::open(file, direct_io)?;
        let mut file_offset: u64 = 0;

        #[cfg(feature = "linux-direct-io")]
        let input_fd = reader.raw_fd();

        while let Some(mut frame) = read_raw_frame(&mut reader, &mut file_offset)? {
            match &frame.blob_type {
                BlobKind::OsmHeader => {}
                BlobKind::OsmData => {
                    #[cfg(feature = "linux-direct-io")]
                    writer.write_raw_copy(
                        input_fd,
                        frame.file_offset,
                        frame.frame_bytes.len() as u64,
                    )?;
                    #[cfg(not(feature = "linux-direct-io"))]
                    writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
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
    filter_node: bool,
    filter_way: bool,
    filter_relation: bool,
) -> std::result::Result<u64, String> {
    let mut count: u64 = 0;

    // Reusable buffers — same hoisting strategy as the old sequential path.
    // Grow to max element size in the block then stabilize.
    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) if filter_node => {
                if !bb.can_add_node() {
                    flush_local(bb, output)?;
                }
                tags_buf.clear();
                tags_buf.extend(dn.tags());
                let meta = dense_node_metadata(dn);
                bb.add_node(
                    dn.id(),
                    dn.decimicro_lat(),
                    dn.decimicro_lon(),
                    &tags_buf,
                    meta.as_ref(),
                );
                count += 1;
            }
            Element::Node(n) if filter_node => {
                if !bb.can_add_node() {
                    flush_local(bb, output)?;
                }
                tags_buf.clear();
                tags_buf.extend(n.tags());
                let meta = element_metadata(&n.info());
                bb.add_node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    &tags_buf,
                    meta.as_ref(),
                );
                count += 1;
            }
            Element::Way(w) if filter_way => {
                if !bb.can_add_way() {
                    flush_local(bb, output)?;
                }
                tags_buf.clear();
                tags_buf.extend(w.tags());
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = element_metadata(&w.info());
                bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                count += 1;
            }
            Element::Relation(r) if filter_relation => {
                if !bb.can_add_relation() {
                    flush_local(bb, output)?;
                }
                tags_buf.clear();
                tags_buf.extend(r.tags());
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                count += 1;
            }
            _ => {}
        }
    }

    Ok(count)
}

#[allow(clippy::too_many_lines)]
fn cat_filtered(files: &[&Path], output: &Path, filter: &str, compression: Compression, direct_io: bool) -> Result<CatStats> {
    let tf = TypeFilter::parse(filter);

    let single_file = files.len() == 1;
    let blob_filter = BlobFilter::new(tf.nodes, tf.ways, tf.relations);

    // -----------------------------------------------------------------------
    // Read header from first file
    // -----------------------------------------------------------------------
    let first_reader = ElementReader::open(files[0], direct_io)?;
    let header = first_reader.header().clone();
    let mut hb = HeaderBuilder::from_header(&header);
    if single_file && header.is_sorted() {
        hb = hb.sorted();
    }
    let header_bytes = hb.build()?;

    let mut writer = PbfWriter::to_path(output, compression, &header_bytes)?;
    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;

    // -----------------------------------------------------------------------
    // Process each input file
    // -----------------------------------------------------------------------
    for file in files {
        let reader = ElementReader::open(file, direct_io)?;
        let blocks_iter = reader.with_blob_filter(blob_filter.clone()).into_blocks_pipelined();

        // Collect decoded blocks into batches of BATCH_SIZE, then process
        // each batch in parallel via rayon.
        let mut batch: Vec<crate::PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

        for block_result in blocks_iter {
            let block = block_result?;
            batch.push(block);

            if batch.len() >= BATCH_SIZE {
                let (batch_blobs, batch_elements) = process_batch(
                    &batch, &mut writer, tf.nodes, tf.ways, tf.relations,
                )?;
                blobs_decoded += batch_blobs;
                elements += batch_elements;
                batch.clear();
            }
        }

        // Flush remaining blocks in the final partial batch
        if !batch.is_empty() {
            let (batch_blobs, batch_elements) = process_batch(
                &batch, &mut writer, tf.nodes, tf.ways, tf.relations,
            )?;
            blobs_decoded += batch_blobs;
            elements += batch_elements;
        }
    }

    writer.flush()?;

    Ok(CatStats {
        blobs_passthrough: 0,
        blobs_decoded,
        elements_written: elements,
    })
}

/// Process a batch of `PrimitiveBlock`s in parallel via rayon.
///
/// Each rayon worker thread owns a `BlockBuilder` (via `map_init`) and
/// processes one block at a time, flushing serialized output to a local
/// `Vec<OwnedBlock>`. After parallel processing, the serialized
/// blocks are written sequentially to the `PbfWriter` in batch order.
///
/// Returns `(blobs_decoded, elements_written)`.
fn process_batch(
    batch: &[crate::PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    filter_node: bool,
    filter_way: bool,
    filter_relation: bool,
) -> Result<(u64, u64)> {
    // Parallel phase: each rayon thread processes one block, returning
    // serialized block bytes (with BlobIndex + tagdata) + element count.
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, u64), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let count = process_block(
                    block, bb, &mut output,
                    filter_node, filter_way, filter_relation,
                )?;
                // Flush any remaining partial block from this thread's builder.
                // The builder is reused across blocks within this batch, so we
                // must drain it after each block to avoid mixing elements from
                // different source blocks.
                flush_local(bb, &mut output)?;
                Ok((output, count))
            },
        )
        .collect();

    // Sequential phase: write serialized blocks in order, propagate errors.
    let mut total_blobs: u64 = 0;
    let mut total_elements: u64 = 0;
    drain_batch_results(results, writer, |count| {
        total_blobs += 1;
        total_elements += count;
    })?;

    Ok((total_blobs, total_elements))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

