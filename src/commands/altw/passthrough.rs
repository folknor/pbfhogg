//! Pass 2b: passthrough output path (indexdata present).
//!
//! Relation blobs and optionally node blobs bypass decode and re-encode -
//! the raw frame bytes either go through a userspace coalescing buffer or,
//! on linux-direct-io, through `copy_file_range` kernel-space copies.
//! Way blobs and decode-required nodes go through a parallel decode batch.

use std::io::Read;
use std::path::Path;

use rayon::prelude::*;

use crate::blob::{
    decode_blob_to_headerblock, decompress_blob, parse_blob_header_with_index,
    parse_primitive_block_from_bytes_owned, BlobKind, DecompressPool, WireBlob,
};
use crate::blob_meta::{BlobIndex, ElemKind};
use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::file_reader::FileReader;
use crate::idset::IdSet;
use crate::read::raw_frame::{read_raw_frame, RawBlobFrame};
use crate::writer::{Compression, PbfWriter};

use crate::commands::{
    build_output_header, drain_batch_results, flush_local, flush_passthrough_buf,
    writer_from_header_bytes, HeaderOverrides, BATCH_BYTE_BUDGET, BATCH_MAX_BLOBS,
    BATCH_MIN_BLOBS,
};

use super::{process_block, NodeIndex, Stats};
use super::Result;

// ---------------------------------------------------------------------------
// Two-phase read: header-only classification + selective data read/skip
// ---------------------------------------------------------------------------

/// Blob header info from phase 1 of two-phase read.
///
/// Contains classification data (blob_type, index) and file position info
/// needed to either read the full blob data or skip it for copy_file_range.
struct BlobHeaderInfo {
    blob_type: BlobKind,
    data_size: usize,
    index: Option<BlobIndex>,
    #[allow(dead_code)]
    tagdata: Option<Box<[u8]>>,
    /// File offset where this frame starts (for copy_file_range).
    frame_start: u64,
    /// Total frame length: 4 + header_len + data_size.
    frame_len: usize,
    /// Raw header prefix: [len_buf(4) | header_bytes(header_len)].
    /// Used by `read_blob_data` to assemble the full frame.
    header_raw: Vec<u8>,
}

/// Read just the BlobHeader (phase 1). Returns `None` at EOF.
///
/// Advances `file_offset` by the header portion only (4 + header_len).
/// The blob data is NOT read - call `read_blob_data` or `skip_blob_data` next.
fn read_blob_header(
    reader: &mut FileReader,
    file_offset: &mut u64,
) -> Result<Option<BlobHeaderInfo>> {
    let frame_start = *file_offset;

    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes(len_buf) as usize;

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;
    let (blob_type, data_size, indexdata, tagdata) =
        parse_blob_header_with_index(&header_bytes)?;

    let blob_offset = 4 + header_len;
    let frame_len = blob_offset + data_size;
    *file_offset += blob_offset as u64;

    let index = indexdata.and_then(|d| BlobIndex::deserialize(&d));

    // Assemble header_raw: [len_buf | header_bytes]
    let mut header_raw = Vec::with_capacity(blob_offset);
    header_raw.extend_from_slice(&len_buf);
    header_raw.extend_from_slice(&header_bytes);

    Ok(Some(BlobHeaderInfo {
        blob_type,
        data_size,
        index,
        tagdata,
        frame_start,
        frame_len,
        header_raw,
    }))
}

/// Read blob data after a header read (phase 2, decode path).
///
/// Consumes the `BlobHeaderInfo` and reads the blob data to produce a full
/// `RawBlobFrame`. Advances `file_offset` by `data_size`.
fn read_blob_data(
    reader: &mut FileReader,
    info: BlobHeaderInfo,
    file_offset: &mut u64,
) -> Result<RawBlobFrame> {
    let blob_offset = info.header_raw.len();
    let mut frame_bytes = Vec::with_capacity(info.frame_len);
    frame_bytes.extend_from_slice(&info.header_raw);
    frame_bytes.resize(info.frame_len, 0);
    reader.read_exact(&mut frame_bytes[blob_offset..])?;
    *file_offset += info.data_size as u64;

    Ok(RawBlobFrame {
        frame_bytes,
        blob_type: info.blob_type,
        blob_offset,
        index: info.index,
        tagdata: info.tagdata,
        file_offset: info.frame_start,
    })
}

/// Skip blob data after a header read (phase 2, passthrough path).
///
/// Advances the reader past the blob data without allocating or reading it
/// into userspace. Advances `file_offset` by `data_size`.
fn skip_blob_data(
    reader: &mut FileReader,
    data_size: usize,
    file_offset: &mut u64,
) -> Result<()> {
    reader.skip(data_size as u64)?;
    *file_offset += data_size as u64;
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch slot for parallel decode
// ---------------------------------------------------------------------------

/// A slot in a parallel decode batch for the passthrough path.
enum BatchSlot {
    /// Way blob: decompress, enrich with node locations, re-encode.
    Way(RawBlobFrame),
    /// Node blob: decompress, filter untagged, re-encode.
    Node(RawBlobFrame),
    /// Unknown blob (no indexdata): decompress, inspect, process generically.
    Unknown(RawBlobFrame),
}

impl BatchSlot {
    fn frame(&self) -> &RawBlobFrame {
        match self {
            Self::Way(f) | Self::Node(f) | Self::Unknown(f) => f,
        }
    }
}

// ---------------------------------------------------------------------------
// Passthrough coalescing
// ---------------------------------------------------------------------------

fn coalesce_passthrough(frame: &mut RawBlobFrame, chunks: &mut Vec<Vec<u8>>) {
    chunks.push(std::mem::take(&mut frame.frame_bytes));
}

// ---------------------------------------------------------------------------
// Copy-range passthrough (linux-direct-io: kernel-space copy via copy_file_range)
// ---------------------------------------------------------------------------

/// Coalesced file range for kernel-space passthrough copy.
///
/// Consecutive passthrough blobs produce contiguous byte ranges in the input
/// file. Rather than issuing a `write_raw_copy` per blob (like merge), we
/// extend the range and flush once per contiguous run. At planet scale,
/// hundreds of consecutive passthrough blobs are common.
#[cfg(feature = "linux-direct-io")]
struct CopyRange {
    input_fd: std::os::unix::io::RawFd,
    start: u64,
    len: u64,
}

#[cfg(feature = "linux-direct-io")]
impl CopyRange {
    fn new(input_fd: std::os::unix::io::RawFd) -> Self {
        Self { input_fd, start: 0, len: 0 }
    }

    fn extend(&mut self, frame_start: u64, frame_len: u64) {
        if self.len == 0 {
            self.start = frame_start;
            self.len = frame_len;
        } else {
            debug_assert_eq!(self.start + self.len, frame_start);
            self.len += frame_len;
        }
    }

    fn flush(
        &mut self,
        writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    ) -> Result<()> {
        if self.len > 0 {
            writer.write_raw_copy(self.input_fd, self.start, self.len)?;
            self.len = 0;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pass 2b: Passthrough path (indexdata present)
// ---------------------------------------------------------------------------

/// Read raw header blob, build output header with `LocationsOnWays`.
fn read_header_raw<R: Read>(
    reader: &mut R,
    file_offset: &mut u64,
    overrides: &HeaderOverrides,
) -> Result<(Vec<u8>, bool)> {
    while let Some(frame) = read_raw_frame(reader, file_offset)? {
        if frame.blob_type == BlobKind::OsmHeader {
            let header = decode_blob_to_headerblock(frame.blob_bytes())?;
            let sorted = header.is_sorted();
            let header_bytes = build_output_header(&header, true, overrides, |hb| {
                hb.optional_feature("LocationsOnWays")
            })?;
            return Ok((header_bytes, sorted));
        }
    }
    Err("no OSMHeader blob found".into())
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn write_output_passthrough(
    input: &Path,
    output: &Path,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    let mut stats = Stats::default();

    let mut reader = FileReader::open(input, direct_io)?;
    let mut file_offset: u64 = 0;
    let (header_bytes, _sorted) = read_header_raw(&mut reader, &mut file_offset, overrides)?;
    let mut writer = writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;

    // Open second handle for copy_file_range (explicit offsets, thread-safe).
    #[cfg(feature = "linux-direct-io")]
    let (_copy_fd_file, use_copy_range) = {
        let f = FileReader::buffered(input)?;
        (f, !direct_io)
    };
    #[cfg(feature = "linux-direct-io")]
    let mut copy_range = {
        let fd = _copy_fd_file.raw_fd();
        CopyRange::new(fd)
    };

    let mut batch: Vec<BatchSlot> = Vec::with_capacity(BATCH_MAX_BLOBS);
    let mut batch_bytes: usize = 0;
    // Coalescing buffer for non-copy-range passthrough (without linux-direct-io,
    // or when copy_file_range is incompatible with O_DIRECT output).
    let mut passthrough_chunks: Vec<Vec<u8>> = Vec::new();
    // Per-batch counter survives SIGKILL inside pass 2; sidecar reads
    // the latest value to know how far the loop got.
    let mut batches_dispatched: i64 = 0;

    while let Some(header) = read_blob_header(&mut reader, &mut file_offset)? {
        if header.blob_type != BlobKind::OsmData {
            skip_blob_data(&mut reader, header.data_size, &mut file_offset)?;
            continue;
        }

        let kind = header.index.as_ref().map(|idx| idx.kind);
        // `kind == None` happens on `--force` against non-indexed input.
        // Both match arms below require `Some(...)`, so `is_passthrough`
        // is false and the blob falls into the decode batch path below,
        // where ordering is preserved implicitly (file-order decode).
        // The "flush before passthrough" invariant is therefore
        // vacuously satisfied for the non-indexed --force case.
        let is_passthrough = matches!(kind, Some(ElemKind::Relation))
            || matches!(kind, Some(ElemKind::Node) if keep_untagged_nodes);

        if is_passthrough {
            // Flush pending decode batch before writing passthrough blobs to
            // preserve input element ordering (nodes → ways → relations).
            // Without this, the last decode batch (ways) could be written after
            // passthrough blobs (relations) at the type boundary.
            if !batch.is_empty() {
                #[cfg(feature = "linux-direct-io")]
                copy_range.flush(&mut writer)?;
                flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                let batch_stats = process_slot_batch(
                    &batch,
                    &mut writer,
                    node_index,
                    keep_untagged_nodes,
                    relation_member_node_ids,
                )?;
                stats.merge(&batch_stats);
                batch.clear();
                batch_bytes = 0;
                batches_dispatched += 1;
                crate::debug::emit_counter("altw_pass2_batches_dispatched", batches_dispatched);
            }

            // Update stats from indexdata.
            if let Some(ref idx) = header.index {
                match idx.kind {
                    ElemKind::Node => {
                        stats.nodes_read += idx.count;
                        stats.nodes_written += idx.count;
                    }
                    ElemKind::Relation => {
                        stats.relations_written += idx.count;
                    }
                    ElemKind::Way => {}
                }
            }
            stats.blobs_passthrough += 1;

            // With copy_file_range: skip blob data, extend kernel copy range.
            // Without: read full frame and coalesce into userspace buffer.
            #[cfg(feature = "linux-direct-io")]
            if use_copy_range {
                skip_blob_data(&mut reader, header.data_size, &mut file_offset)?;
                copy_range.extend(header.frame_start, header.frame_len as u64);
            }
            #[cfg(feature = "linux-direct-io")]
            if !use_copy_range {
                let mut frame = read_blob_data(&mut reader, header, &mut file_offset)?;
                coalesce_passthrough(&mut frame, &mut passthrough_chunks);
            }
            #[cfg(not(feature = "linux-direct-io"))]
            {
                let mut frame = read_blob_data(&mut reader, header, &mut file_offset)?;
                coalesce_passthrough(&mut frame, &mut passthrough_chunks);
            }
        } else {
            // Flush any pending copy range before decoding - the next passthrough
            // blob may not be contiguous with the previous one (decode blobs in
            // between break contiguity).
            #[cfg(feature = "linux-direct-io")]
            copy_range.flush(&mut writer)?;
            flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
            // Decode: read full frame, classify into batch slot.
            let frame = read_blob_data(&mut reader, header, &mut file_offset)?;
            stats.blobs_decoded += 1;
            batch_bytes += frame.frame_bytes.len();
            match kind {
                Some(ElemKind::Node) => batch.push(BatchSlot::Node(frame)),
                Some(ElemKind::Way) => batch.push(BatchSlot::Way(frame)),
                _ => batch.push(BatchSlot::Unknown(frame)),
            }
        }

        // Dispatch when byte budget reached or batch is full.
        if batch.len() >= BATCH_MAX_BLOBS
            || (batch.len() >= BATCH_MIN_BLOBS && batch_bytes >= BATCH_BYTE_BUDGET)
        {
            #[cfg(feature = "linux-direct-io")]
            copy_range.flush(&mut writer)?;
            flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
            let batch_stats = process_slot_batch(
                &batch,
                &mut writer,
                node_index,
                keep_untagged_nodes,
                relation_member_node_ids,
            )?;
            stats.merge(&batch_stats);
            batch.clear();
            batch_bytes = 0;
            batches_dispatched += 1;
            crate::debug::emit_counter("altw_pass2_batches_dispatched", batches_dispatched);
        }
    }

    // Flush remaining decode batch, then passthrough.
    if !batch.is_empty() {
        let batch_stats = process_slot_batch(
            &batch,
            &mut writer,
            node_index,
            keep_untagged_nodes,
            relation_member_node_ids,
        )?;
        stats.merge(&batch_stats);
        batches_dispatched += 1;
        crate::debug::emit_counter("altw_pass2_batches_dispatched", batches_dispatched);
    }
    #[cfg(feature = "linux-direct-io")]
    copy_range.flush(&mut writer)?;
    flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;

    writer.flush()?;
    Ok(stats)
}

/// Process a batch of slots in parallel: decompress, transform, write.
///
/// One par_iter pass per slot: decompress + parse + process_block +
/// flush_local. Way refs resolve via inline `NodeIndex::get` against
/// either the dense or sparse index. The previous sparse-only
/// pre-resolve (decompress all, then sort refs by mmap offset, then
/// sequential scan) was a serial step that capped pass 2 at ~4 cores;
/// inline lookups scale with the rayon worker count instead.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn process_slot_batch(
    batch: &[BatchSlot],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
) -> Result<Stats> {
    type SlotResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;

    let results: Vec<SlotResult> = batch
        .par_iter()
        .map_init(
            || {
                (
                    BlockBuilder::new(),
                    Vec::<OwnedBlock>::new(),
                    Vec::<i64>::new(),
                    Vec::<(i32, i32)>::new(),
                    DecompressPool::new(),
                )
            },
            |(bb, output, refs_buf, locations_buf, pool), slot| {
                output.clear();

                let wire_blob = WireBlob::parse_slice(slot.frame().blob_bytes())
                    .map_err(|e| e.to_string())?;
                let bytes = decompress_blob(&wire_blob, Some(pool))
                    .map_err(|e| e.to_string())?;
                let block = parse_primitive_block_from_bytes_owned(&bytes)
                    .map_err(|e| e.to_string())?;

                let block_stats = process_block(
                    &block, bb, output, node_index,
                    keep_untagged_nodes, relation_member_node_ids,
                    refs_buf, locations_buf,
                )?;

                flush_local(bb, output)?;
                Ok((std::mem::take(output), block_stats))
            },
        )
        .collect();

    let mut total = Stats {
        nodes_read: 0, nodes_written: 0, nodes_dropped: 0,
        ways_written: 0, relations_written: 0, missing_locations: 0,
        blobs_passthrough: 0, blobs_decoded: 0,
    };
    drain_batch_results(results, writer, |s| total.merge(&s))?;
    Ok(total)
}
