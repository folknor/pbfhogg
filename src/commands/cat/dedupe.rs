//! Merge multiple sorted PBF files into one. Equivalent to `osmium merge`.
//!
//! Two-pass blob-level merge:
//!   Pass 1: Build blob index from all input files
//!   Pass 2: Write in sorted order - passthrough for non-overlapping blobs,
//!           decode + sweep merge with dedup for overlapping blobs.
//!
//! Exact duplicates (same type, ID, version) across input files are removed.
//! Common when merging geographic extracts that share border elements.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::blob::{
    decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
    parse_blob_header_with_index, BlobKind,
};
use crate::blob_meta::{BlobIndex, ElemKind, scan_block_ids};
use crate::block_builder::BlockBuilder;
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::writer::{reframe_raw_with_index, Compression, PbfWriter};
use crate::Element;

use crate::owned::{
    read_dense_node, read_node, read_relation, read_way, write_single_node, write_single_relation,
    write_single_way, OwnedNode, OwnedRelation, OwnedWay,
};
use crate::commands::{
    build_output_header, flush_block, require_indexdata, require_sorted, writer_from_header_bytes,
    HeaderOverrides,
};
use crate::BoxResult as Result;

/// Statistics from a multi-PBF merge operation.
pub struct MergePbfStats {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    pub blobs_passthrough: u64,
    pub blobs_rewritten: u64,
    pub blobs_total: u64,
    pub duplicates_removed: u64,
}

impl MergePbfStats {
    pub fn print_summary(&self) {
        let total = self.nodes + self.ways + self.relations;
        eprintln!(
            "Merged {total} elements: {} nodes, {} ways, {} relations \
             ({} blobs: {} passthrough, {} rewritten, {} duplicates removed)",
            self.nodes,
            self.ways,
            self.relations,
            self.blobs_total,
            self.blobs_passthrough,
            self.blobs_rewritten,
            self.duplicates_removed,
        );
    }
}

/// Options for the multi-PBF merge command.
pub struct MergePbfOptions {
    pub compression: Compression,
    pub direct_io: bool,
    pub io_uring: bool,
    pub force: bool,
}

// ---------------------------------------------------------------------------
// Blob index entry
// ---------------------------------------------------------------------------

struct BlobEntry {
    file_offset: u64,
    frame_len: u64,
    index: BlobIndex,
    has_indexdata: bool,
    tagdata: Option<Box<[u8]>>,
    file_index: usize,
}


trait HasId {
    fn id(&self) -> i64;
    fn version(&self) -> i32;
}

impl HasId for OwnedNode {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

impl HasId for OwnedWay {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

impl HasId for OwnedRelation {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Merge multiple sorted PBF files into one, deduplicating exact duplicates.
///
/// All inputs must be sorted (Sort.Type_then_ID). Uses blob-level index for
/// fast passthrough of non-overlapping blobs; overlapping blobs are decoded,
/// merged, and deduped via a streaming sweep.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn merge_pbf(
    inputs: &[&Path],
    output: &Path,
    opts: &MergePbfOptions,
    overrides: &HeaderOverrides,
) -> Result<MergePbfStats> {
    if inputs.is_empty() {
        return Err("no input files specified".into());
    }

    // Validate inputs have indexdata
    for input in inputs {
        require_indexdata(
            input,
            opts.direct_io,
            opts.force,
            "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
             decompressed to scan element IDs (significantly slower).",
        )?;
    }

    crate::debug::emit_marker("MERGEPBF_START");
    // Pass 1: Build blob index from all files
    eprintln!("Pass 1: indexing blobs...");
    let (header, mut entries) = build_blob_index(inputs, opts.direct_io)?;
    crate::commands::warn_locations_on_ways_loss(&header);
    eprintln!(
        "  {} OSMData blobs indexed from {} files",
        entries.len(),
        inputs.len()
    );

    // Sort by (type, osm_id)
    entries.sort_by_key(|e| {
        (
            type_order(e.index.kind),
            crate::osm_id::blob_osm_first_key(e.index.min_id, e.index.max_id),
        )
    });

    // Detect overlaps
    let overlaps = detect_overlaps(&entries);
    let overlap_count = overlaps.iter().filter(|&&b| b).count();
    if overlap_count > 0 {
        eprintln!("  {overlap_count} blobs in overlap runs (decode + merge + dedup)");
    }

    // Pass 2: Write in sorted order
    eprintln!("Pass 2: writing merged output...");
    #[allow(clippy::redundant_closure_for_method_calls)]
    let header_bytes = build_output_header(&header, false, overrides, |hb| hb.sorted())?;
    let mut writer = writer_from_header_bytes(
        output,
        opts.compression,
        &header_bytes,
        opts.direct_io,
        opts.io_uring,
    )?;

    // Open all input files for random access
    let mut files: Vec<File> = inputs
        .iter()
        .map(File::open)
        .collect::<io::Result<_>>()?;

    let mut stats = MergePbfStats {
        nodes: 0,
        ways: 0,
        relations: 0,
        blobs_passthrough: 0,
        blobs_rewritten: 0,
        blobs_total: entries.len() as u64,
        duplicates_removed: 0,
    };

    let mut bb = BlockBuilder::new();
    let mut frame_buf: Vec<u8> = Vec::new();

    let mut i = 0;
    while i < entries.len() {
        if overlaps[i] {
            let start = i;
            let run_kind = entries[i].index.kind;
            // Overlap runs must contain exactly one element kind.
            // `detect_overlaps` only sets `overlaps[j]` based on same-kind
            // adjacency, so the only way a kind boundary lands mid-run
            // here is when two same-kind overlap-runs sit adjacent in
            // file order (e.g. a node overlap-pair followed immediately
            // by a way overlap-pair). Grouping those into one
            // `write_overlap_run` call hands `entries[0].index.kind` to
            // `sweep_merge_dedup`, whose kind-gated extract closure then
            // silently drops every element whose kind doesn't match -
            // the exact shape of the `merge_pbf([A, A])` regression
            // where ways and relations disappeared.
            while i < entries.len() && overlaps[i] && entries[i].index.kind == run_kind {
                i += 1;
            }
            write_overlap_run(
                &entries[start..i],
                &mut files,
                &mut bb,
                &mut writer,
                &mut stats,
            )?;
        } else {
            write_passthrough_blob(
                &entries[i],
                &mut files[entries[i].file_index],
                &mut writer,
                &mut frame_buf,
            )?;
            count_entry(&entries[i], &mut stats);
            stats.blobs_passthrough += 1;
            i += 1;
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    crate::debug::emit_marker("MERGEPBF_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Pass 1: Build blob index from multiple files
// ---------------------------------------------------------------------------

fn build_blob_index(
    inputs: &[&Path],
    direct_io: bool,
) -> Result<(crate::HeaderBlock, Vec<BlobEntry>)> {
    let mut entries = Vec::new();
    let mut header: Option<crate::HeaderBlock> = None;
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut header_buf: Vec<u8> = Vec::new();
    let mut blob_buf: Vec<u8> = Vec::new();

    for (file_idx, input) in inputs.iter().enumerate() {
        let mut reader = FileReader::open(input, direct_io)?;
        let mut file_offset: u64 = 0;

        loop {
            let frame_start = file_offset;

            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            #[allow(clippy::cast_possible_truncation)]
            let header_len = u32::from_be_bytes(len_buf) as usize;

            header_buf.resize(header_len, 0);
            reader.read_exact(&mut header_buf)?;

            let (blob_type, data_size, raw_index, tagdata) =
                parse_blob_header_with_index(&header_buf)?;
            let index = raw_index.as_ref().and_then(|d| BlobIndex::deserialize(d));
            let has_indexdata = index.is_some();

            let frame_len = (4 + header_len + data_size) as u64;
            file_offset += frame_len;

            match &blob_type {
                BlobKind::OsmHeader => {
                    blob_buf.resize(data_size, 0);
                    reader.read_exact(&mut blob_buf)?;
                    let hdr = decode_blob_to_headerblock(&blob_buf)?;
                    require_sorted(&hdr, input, &format!("Input file {}", input.display()))?;
                    if header.is_none() {
                        header = Some(hdr);
                    }
                }
                BlobKind::OsmData if has_indexdata => {
                    reader.skip(data_size as u64)?;
                    #[allow(clippy::unwrap_used)]
                    entries.push(BlobEntry {
                        file_offset: frame_start,
                        frame_len,
                        index: index.unwrap(),
                        has_indexdata,
                        tagdata,
                        file_index: file_idx,
                    });
                }
                BlobKind::OsmData => {
                    blob_buf.resize(data_size, 0);
                    reader.read_exact(&mut blob_buf)?;
                    decompress_blob_data_into(&blob_buf, &mut decompress_buf)?;
                    let blob_index = scan_block_ids(&decompress_buf).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "failed to scan block IDs")
                    })?;
                    entries.push(BlobEntry {
                        file_offset: frame_start,
                        frame_len,
                        index: blob_index,
                        has_indexdata,
                        tagdata,
                        file_index: file_idx,
                    });
                }
                _ => {
                    reader.skip(data_size as u64)?;
                }
            }
        }
    }

    let header = header.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "no OSMHeader blob found in any input file")
    })?;

    Ok((header, entries))
}

// ---------------------------------------------------------------------------
// Sort + overlap detection
// ---------------------------------------------------------------------------

fn type_order(kind: ElemKind) -> u8 {
    match kind {
        ElemKind::Node => 0,
        ElemKind::Way => 1,
        ElemKind::Relation => 2,
    }
}

fn detect_overlaps(entries: &[BlobEntry]) -> Vec<bool> {
    let mut overlaps = vec![false; entries.len()];
    for i in 0..entries.len().saturating_sub(1) {
        if entries[i].index.kind == entries[i + 1].index.kind
            && crate::osm_id::blob_osm_last_key(entries[i].index.min_id, entries[i].index.max_id)
                >= crate::osm_id::blob_osm_first_key(
                    entries[i + 1].index.min_id,
                    entries[i + 1].index.max_id,
                )
        {
            overlaps[i] = true;
            overlaps[i + 1] = true;
        }
    }
    overlaps
}

// ---------------------------------------------------------------------------
// Pass 2: Passthrough write
// ---------------------------------------------------------------------------

fn write_passthrough_blob(
    entry: &BlobEntry,
    file: &mut File,
    writer: &mut PbfWriter<FileWriter>,
    frame_buf: &mut Vec<u8>,
) -> Result<()> {
    read_frame_into(file, entry, frame_buf)?;
    if entry.has_indexdata {
        writer.write_raw(frame_buf)?;
    } else {
        let blob_bytes = extract_blob_bytes(frame_buf)?;
        let reframed = reframe_raw_with_index(
            blob_bytes,
            &entry.index.serialize(),
            entry.tagdata.as_deref(),
        )?;
        writer.write_raw(&reframed)?;
    }
    Ok(())
}

fn read_frame_into(file: &mut File, entry: &BlobEntry, buf: &mut Vec<u8>) -> io::Result<()> {
    file.seek(SeekFrom::Start(entry.file_offset))?;
    #[allow(clippy::cast_possible_truncation)]
    let len = entry.frame_len as usize;
    buf.clear();
    buf.resize(len, 0);
    file.read_exact(buf)?;
    Ok(())
}

fn extract_blob_bytes(frame: &[u8]) -> Result<&[u8]> {
    if frame.len() < 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too short").into());
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let blob_start = 4 + header_len;
    if blob_start > frame.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid header length").into());
    }
    Ok(&frame[blob_start..])
}

fn count_entry(entry: &BlobEntry, stats: &mut MergePbfStats) {
    match entry.index.kind {
        ElemKind::Node => stats.nodes += entry.index.count,
        ElemKind::Way => stats.ways += entry.index.count,
        ElemKind::Relation => stats.relations += entry.index.count,
    }
}

// ---------------------------------------------------------------------------
// Pass 2: Overlap run rewrite with dedup
// ---------------------------------------------------------------------------

fn write_overlap_run(
    entries: &[BlobEntry],
    files: &mut [File],
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergePbfStats,
) -> Result<()> {
    let kind = entries[0].index.kind;
    match kind {
        ElemKind::Node => {
            let (c, d) = sweep_merge_dedup(entries, files, bb, writer,
                |e, heap| match e {
                    Element::DenseNode(dn) => heap.push(Reverse(read_dense_node(dn))),
                    Element::Node(n) => heap.push(Reverse(read_node(n))),
                    _ => {}
                },
                write_single_node,
            )?;
            stats.nodes += c;
            stats.duplicates_removed += d;
        }
        ElemKind::Way => {
            let (c, d) = sweep_merge_dedup(entries, files, bb, writer,
                |e, heap| { if let Element::Way(w) = e { heap.push(Reverse(read_way(w))); } },
                write_single_way,
            )?;
            stats.ways += c;
            stats.duplicates_removed += d;
        }
        ElemKind::Relation => {
            let (c, d) = sweep_merge_dedup(entries, files, bb, writer,
                |e, heap| { if let Element::Relation(r) = e { heap.push(Reverse(read_relation(r))); } },
                write_single_relation,
            )?;
            stats.relations += c;
            stats.duplicates_removed += d;
        }
    };
    stats.blobs_rewritten += entries.len() as u64;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sweep merge per element type - with dedup
// ---------------------------------------------------------------------------

/// Generic sweep merge: heap-based merge of overlapping blob entries with
/// dedup (skip elements with same id + version as the just-emitted element).
/// Returns (elements_written, duplicates_removed).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn sweep_merge_dedup<T: Ord + HasId>(
    entries: &[BlobEntry],
    files: &mut [File],
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    mut extract: impl FnMut(&Element<'_>, &mut BinaryHeap<Reverse<T>>),
    mut write_elem: impl FnMut(&T, &mut BlockBuilder, &mut PbfWriter<FileWriter>) -> Result<()>,
) -> Result<(u64, u64)> {
    let mut heap: BinaryHeap<Reverse<T>> = BinaryHeap::new();
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut count: u64 = 0;
    let mut deduped: u64 = 0;

    for entry in entries {
        flush_heap_below_dedup(&mut heap, crate::osm_id::blob_osm_first_id(entry.index.min_id, entry.index.max_id), &mut deduped, |elem| {
            write_elem(&elem, bb, writer)?;
            count += 1;
            Ok(())
        })?;

        read_frame_into(&mut files[entry.file_index], entry, &mut frame_buf)?;
        let blob_bytes = extract_blob_bytes(&frame_buf)?;
        let block = decode_blob_to_primitiveblock(blob_bytes)?;
        for element in block.elements() {
            extract(&element, &mut heap);
        }
    }

    // Drain remaining with dedup
    while let Some(Reverse(elem)) = heap.pop() {
        let eid = elem.id();
        let ever = elem.version();
        write_elem(&elem, bb, writer)?;
        count += 1;
        while heap
            .peek()
            .is_some_and(|Reverse(e)| e.id() == eid && e.version() == ever)
        {
            heap.pop();
            deduped += 1;
        }
    }
    flush_block(bb, writer)?;
    Ok((count, deduped))
}

/// Flush elements from the min-heap whose ID is below `below`, with dedup.
fn flush_heap_below_dedup<T: Ord + HasId>(
    heap: &mut BinaryHeap<Reverse<T>>,
    below: i64,
    dedup_count: &mut u64,
    mut emit: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    while heap
        .peek()
        .is_some_and(|Reverse(e)| crate::osm_id::osm_id_cmp(e.id(), below).is_lt())
    {
        if let Some(Reverse(element)) = heap.pop() {
            let eid = element.id();
            let ever = element.version();
            emit(element)?;
            // Skip exact duplicates (same id + version)
            while heap
                .peek()
                .is_some_and(|Reverse(e)| e.id() == eid && e.version() == ever)
            {
                heap.pop();
                *dedup_count += 1;
            }
        }
    }
    Ok(())
}

