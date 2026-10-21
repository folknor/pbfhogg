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

use crate::Element;
use crate::blob::{
    BlobKind, decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
};
use crate::blob_meta::{BlobIndex, ElemKind, scan_block_ids};
use crate::block_builder::BlockBuilder;
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::frame::read_frame_header;
use crate::writer::{Compression, PbfWriter, reframe_raw_with_index};

use crate::BoxResult as Result;
use crate::commands::{
    HeaderOverrides, build_output_header, flush_block, require_indexdata, require_sorted,
    writer_from_header_bytes,
};
use crate::owned::{
    OwnedNode, OwnedRelation, OwnedWay, read_dense_node, read_node, read_relation, read_way,
    write_single_node, write_single_relation, write_single_way,
};

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
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self.nodes + self.ways + self.relations;
        writeln!(
            w,
            "Merged {total} elements: {} nodes, {} ways, {} relations \
             ({} blobs: {} passthrough, {} rewritten, {} duplicates removed)",
            self.nodes,
            self.ways,
            self.relations,
            self.blobs_total,
            self.blobs_passthrough,
            self.blobs_rewritten,
            self.duplicates_removed,
        )?;

        Ok(())
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
    host: &crate::host::Host,
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
            host,
        )?;
    }

    crate::debug::emit_marker("MERGEPBF_START");
    // Pass 1: Build blob index from all files
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.pass1",
        "Pass 1: indexing blobs..."
    );
    let (header, mut entries) = build_blob_index(inputs, opts.direct_io, host)?;
    crate::commands::warn_locations_on_ways_loss(&header, host);
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.indexed",
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
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "cat.dedupe.overlap_runs",
            "  {overlap_count} blobs in overlap runs (decode + merge + dedup)"
        );
    }

    // Pass 2: Write in sorted order
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.pass2",
        "Pass 2: writing merged output..."
    );
    #[allow(clippy::redundant_closure_for_method_calls)]
    let header_bytes = build_output_header(&header, false, overrides, |hb| hb.sorted())?;
    let mut writer = writer_from_header_bytes(
        output,
        opts.compression,
        &header_bytes,
        opts.direct_io,
        opts.io_uring,
        host,
    )?;

    // Open all input files for random access
    let mut files: Vec<File> = inputs.iter().map(File::open).collect::<io::Result<_>>()?;

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
                host,
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
    host: &crate::host::Host,
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

            // Prefix, EOF policy, the zero-length and MAX_BLOB_HEADER_SIZE
            // guards, and the header parse all come from `read::frame`. This
            // loop previously resized `header_buf` straight from the
            // unvalidated 4-byte prefix, so four bytes of corrupt input asked
            // for up to a 4 GiB allocation - the one frame decoder that had
            // neither guard.
            let Some(frame) = read_frame_header(&mut reader, &mut header_buf, &host.diagnostics)?
            else {
                break;
            };
            let index = frame
                .raw_index
                .as_ref()
                .and_then(|d| BlobIndex::deserialize(d));
            let has_indexdata = index.is_some();
            let frame_len = (frame.payload_offset() + frame.data_size) as u64;
            let blob_type = frame.blob_type;
            let data_size = frame.data_size;
            let tagdata = frame.tagdata;

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
        io::Error::new(
            io::ErrorKind::InvalidData,
            "no OSMHeader blob found in any input file",
        )
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

/// Flag every blob that belongs to a range-overlap run.
///
/// Entries arrive sorted by first key within a kind. The test is a
/// running-maximum sweep, not a pairwise-adjacent comparison, because overlap
/// is **not transitive**: with A=[1..1000], B=[2..3], C=[500..600], A/B overlap
/// but B/C do not (`B.max=3 < C.min=500`), so a pairwise scan leaves C
/// unflagged. The sweep-merge then emits A's ids 500..1000 and C follows as a
/// raw passthrough blob carrying ids 500..600 - output that is not sorted while
/// the header still stamps `Sort.Type_then_ID`, and (in `cat --dedupe`) A/C
/// duplicates that the "exact duplicates removed" contract promised to drop.
///
/// Extending the run while `next.first_key <= run_max` closes that: once a blob
/// joins a run, every later blob whose first key falls inside the run's widest
/// reach so far joins too. Runs stay kind-bounded, exactly as before - the
/// downstream sweep is kind-gated and silently drops off-kind elements handed
/// to it.
fn detect_overlaps(entries: &[BlobEntry]) -> Vec<bool> {
    let mut overlaps = vec![false; entries.len()];
    let mut i = 0;
    while i < entries.len() {
        let kind = entries[i].index.kind;
        let mut run_max =
            crate::osm_id::blob_osm_last_key(entries[i].index.min_id, entries[i].index.max_id);
        let mut j = i + 1;
        while j < entries.len()
            && entries[j].index.kind == kind
            && crate::osm_id::blob_osm_first_key(entries[j].index.min_id, entries[j].index.max_id)
                <= run_max
        {
            run_max = run_max.max(crate::osm_id::blob_osm_last_key(
                entries[j].index.min_id,
                entries[j].index.max_id,
            ));
            j += 1;
        }
        if j > i + 1 {
            for flag in &mut overlaps[i..j] {
                *flag = true;
            }
            i = j;
        } else {
            i += 1;
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
    host: &crate::host::Host,
) -> Result<()> {
    let kind = entries[0].index.kind;
    match kind {
        ElemKind::Node => {
            let (c, d) = sweep_merge_dedup(
                entries,
                files,
                bb,
                writer,
                |e, heap| match e {
                    Element::DenseNode(dn) => heap.push(Reverse(read_dense_node(dn))),
                    Element::Node(n) => heap.push(Reverse(read_node(n))),
                    _ => {}
                },
                write_single_node,
                host,
            )?;
            stats.nodes += c;
            stats.duplicates_removed += d;
        }
        ElemKind::Way => {
            let (c, d) = sweep_merge_dedup(
                entries,
                files,
                bb,
                writer,
                |e, heap| {
                    if let Element::Way(w) = e {
                        heap.push(Reverse(read_way(w)));
                    }
                },
                write_single_way,
                host,
            )?;
            stats.ways += c;
            stats.duplicates_removed += d;
        }
        ElemKind::Relation => {
            let (c, d) = sweep_merge_dedup(
                entries,
                files,
                bb,
                writer,
                |e, heap| {
                    if let Element::Relation(r) = e {
                        heap.push(Reverse(read_relation(r)));
                    }
                },
                write_single_relation,
                host,
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
    host: &crate::host::Host,
) -> Result<(u64, u64)> {
    let _ = host;
    let mut heap: BinaryHeap<Reverse<T>> = BinaryHeap::new();
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut count: u64 = 0;
    let mut deduped: u64 = 0;

    for entry in entries {
        flush_heap_below_dedup(
            &mut heap,
            crate::osm_id::blob_osm_first_id(entry.index.min_id, entry.index.max_id),
            &mut deduped,
            |elem| {
                write_elem(&elem, bb, writer)?;
                count += 1;
                Ok(())
            },
        )?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ElemKind, min_id: i64, max_id: i64) -> BlobEntry {
        BlobEntry {
            file_offset: 0,
            frame_len: 0,
            index: BlobIndex {
                kind,
                min_id,
                max_id,
                count: 1,
                bbox: None,
            },
            has_indexdata: true,
            tagdata: None,
            file_index: 0,
        }
    }

    /// The transitivity failure, in the merge/dedupe copy. A=[1..1000],
    /// B=[2..3], C=[500..600] in first-key order: A/B overlap, B/C do not, so
    /// a pairwise-adjacent scan leaves C out of the run. C's elements are then
    /// written as a raw passthrough blob and never reach `sweep_merge_dedup`,
    /// so A/C duplicates survive - the "exact duplicates removed" contract
    /// failing silently - and the output is unsorted under a `Sort.Type_then_ID`
    /// stamp. Realistic for merging extracts, whose per-blob ID ranges are wide
    /// and sparse.
    #[test]
    fn overlap_runs_are_transitive_not_pairwise() {
        let entries = vec![
            entry(ElemKind::Node, 1, 1000),
            entry(ElemKind::Node, 2, 3),
            entry(ElemKind::Node, 500, 600),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, true],
            "C=[500..600] lies inside A=[1..1000] and must join the run even \
             though it does not overlap its immediate predecessor B=[2..3]"
        );
    }

    /// Passing counterpart: disjoint blobs must stay on the passthrough path,
    /// which is the whole point of the blob-level merge.
    #[test]
    fn disjoint_blobs_are_not_flagged_as_overlapping() {
        let entries = vec![
            entry(ElemKind::Node, 1, 10),
            entry(ElemKind::Node, 11, 20),
            entry(ElemKind::Node, 21, 30),
        ];
        assert_eq!(detect_overlaps(&entries), vec![false, false, false]);
    }

    #[test]
    fn a_run_never_crosses_a_kind_boundary() {
        let entries = vec![
            entry(ElemKind::Node, 1, 1000),
            entry(ElemKind::Node, 2, 3),
            entry(ElemKind::Way, 5, 6),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, false],
            "handing a mixed-kind run to the kind-gated sweep silently drops \
             the off-kind elements"
        );
    }

    #[test]
    fn a_new_run_starts_after_the_previous_one_ends() {
        let entries = vec![
            entry(ElemKind::Node, 1, 100),
            entry(ElemKind::Node, 50, 60),
            entry(ElemKind::Node, 200, 300),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, false],
            "the blob after a run must be judged on its own, not inherit the \
             run's flag"
        );
    }
}
