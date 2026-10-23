//! Sort a PBF file into standard order. Equivalent to `osmium sort`.
//!
//! Uses blob-level permutation sort: index each blob's element type + ID range,
//! sort the index, write in sorted order. Non-overlapping blobs pass through as
//! raw bytes; overlapping blobs are decoded and merged via a streaming sweep.
//! Memory usage is O(num_blobs) for non-overlapping inputs. Overlapping blobs
//! use a streaming sweep merge with O(overlap_depth) memory.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::Element;
use crate::ElementKind;
use crate::blob::{
    BlobKind, decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
};
use crate::blob_meta::{BlobIndex, scan_block_ids_checked};
use crate::block_builder::BlockBuilder;
use crate::error::Error;
use crate::file_writer::FileWriter;
use crate::read::header_walker::HeaderWalker;
use crate::read::raw_frame::frame_location;
use crate::write::WriteOptions;
use crate::writer::{PbfWriter, reframe_raw_with_index};

use super::{HeaderOverrides, Result, require_indexdata, writer_from_header_bytes};
use crate::block_builder::OwnedBlock;
use crate::owned::{
    OwnedNode, OwnedRelation, OwnedWay, read_dense_node, read_node, read_relation, read_way,
    write_single_node_local, write_single_relation_local, write_single_way_local,
};
use rayon::prelude::*;

/// Statistics from a sort operation.
pub struct SortStats {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    /// Elements in passthrough blobs whose count came from unverified
    /// `BlobHeader.indexdata` rather than from a decode. See
    /// [`BlobEntry::index_scanned`].
    pub passthrough_claimed: u64,
    pub blobs_passthrough: u64,
    pub blobs_rewritten: u64,
    pub blobs_total: u64,
}

impl SortStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Sorted {} nodes, {} ways, {} relations ({} blobs: {} passthrough, {} rewritten)",
            self.nodes,
            self.ways,
            self.relations,
            self.blobs_total,
            self.blobs_passthrough,
            self.blobs_rewritten,
        )?;
        if self.passthrough_claimed > 0 {
            writeln!(
                w,
                "  plus {} elements in blobs copied through without decoding, per \
                 their indexdata (unverified - `pbfhogg check --index` checks it)",
                self.passthrough_claimed,
            )?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Blob index
// ---------------------------------------------------------------------------

/// Blob-level index entry for permutation sort.
struct BlobEntry {
    /// Byte offset of the complete frame in the input file.
    file_offset: u64,
    /// Length of the complete frame (4 + header_len + data_size).
    frame_len: u64,
    /// Element type + ID range.
    index: BlobIndex,
    /// Whether the BlobHeader already has indexdata embedded.
    has_indexdata: bool,
    /// Per-blob tag key data from BlobHeader field 4, preserved for passthrough.
    tagdata: Option<Box<[u8]>>,
    /// True when this blob's elements are internally out of canonical OSM ID
    /// order. Set by the payload-decoding pass-1 scan, which runs for every
    /// non-indexed blob and for indexed blobs whose input header does not
    /// claim `Sort.Type_then_ID`. Only blobs of a declared-sorted input skip
    /// the payload; intra-blob sortedness there is a precondition of the
    /// header claim itself (see CORRECTNESS.md). A blob flagged here is
    /// routed into the decode + re-encode path so the sweep-merge actually
    /// reorders it, even when its (min_id, max_id) range does not overlap
    /// any neighbour.
    intra_unsorted: bool,
    /// True when `index` was derived by decoding the blob's payload, false
    /// when it was read out of the `BlobHeader` and believed.
    ///
    /// Only the element counters need this. A passthrough blob's elements are
    /// never decoded on the declared-sorted fast path, so `index.count` there
    /// is the file's own unverified claim about itself; folding it into
    /// "Sorted N nodes" reported hearsay as observation, and a lying
    /// `indexdata` moved the number with nothing to say so.
    index_scanned: bool,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Sort a PBF file into standard order (nodes -> ways -> relations, by ID).
///
/// Uses blob-level permutation sort: O(num_blobs) memory. Non-overlapping
/// blobs are passed through as raw bytes; overlapping blobs are decoded,
/// sorted, and re-encoded.
pub fn sort(
    input: &Path,
    output: crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<SortStats> {
    let result = sort_to(input, &output, opts, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`sort`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub(crate) fn sort_to(
    input: &Path,
    output: &crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<SortStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        force,
    } = *opts;
    let indexed = require_indexdata(
        input,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed to scan element IDs (significantly slower).",
        host,
    )?;
    if !indexed {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "sort.unindexed_fallback",
            "sorting without indexdata; every blob must be decompressed to scan element IDs"
        );
        crate::debug::emit_counter("sort_unindexed_fallback", 1);
    }

    #[allow(clippy::cast_possible_wrap)]
    if let Ok(meta) = std::fs::metadata(input) {
        crate::debug::emit_counter("sort_total_bytes_in", meta.len() as i64);
    }

    // Pass 1: Build blob index
    crate::debug::emit_marker("SORT_PASS1_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "sort.pass1",
        "Pass 1: indexing blobs..."
    );
    crate::debug::emit_marker("SORT_INDEX_BUILD_START");
    let (header, mut entries) = build_blob_index(input, direct_io, host)?;
    crate::debug::emit_marker("SORT_INDEX_BUILD_END");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "sort.indexed",
        "  {} OSMData blobs indexed",
        entries.len()
    );
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("sort_blobs_total", entries.len() as i64);

    // Sort by (type_order, min_id)
    crate::debug::emit_marker("SORT_OVERLAP_DETECT_START");
    entries.sort_by_key(|e| {
        (
            type_order(e.index.kind),
            crate::osm_id::blob_osm_first_key(e.index.min_id, e.index.max_id),
        )
    });

    // Detect overlaps
    let mut overlaps = detect_overlaps(&entries);
    // Capture the genuine range-overlap count BEFORE folding intra-blob
    // disorder in. `sort_blobs_overlap` keeps its historical meaning:
    // blobs whose (min_id, max_id) range overlaps a same-kind neighbour.
    // Intra-unsorted blobs forced into the same rewrite path below are
    // counted separately (sort_blobs_intra_unsorted) so the two attributions
    // stay disjoint instead of double-counting the forced rewrites.
    let overlap_count = overlaps.iter().filter(|&&b| b).count();
    // Fold in intra-blob disorder detected during the pass-1 payload scan.
    // A blob whose elements are internally out of ID order but whose
    // (min_id, max_id) range does not overlap its neighbours would otherwise
    // pass straight through as raw bytes while the output header still claims
    // Sort.Type_then_ID - silent corruption of the sorted invariant. Route it
    // into the same decode + re-encode path an overlapping blob takes so the
    // sweep-merge reorders its elements. (Blobs of a declared-sorted indexed
    // input are never flagged here; pass 1 skips their payloads and trusts
    // the header's Sort.Type_then_ID claim - see CORRECTNESS.md.)
    let intra_unsorted_blobs = mark_intra_unsorted_for_rewrite(&entries, &mut overlaps);
    crate::debug::emit_marker("SORT_OVERLAP_DETECT_END");
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("sort_blobs_overlap", overlap_count as i64);
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("sort_blobs_intra_unsorted", intra_unsorted_blobs as i64);
    if overlap_count > 0 {
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "sort.overlap_runs",
            "  {overlap_count} blobs in overlap runs (decode + re-encode)"
        );
    }
    if intra_unsorted_blobs > 0 {
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "sort.intra_unsorted",
            "  {intra_unsorted_blobs} blobs internally unsorted (decode + re-encode)"
        );
    }

    crate::debug::emit_marker("SORT_PASS1_END");

    // Pass 2: Write in sorted order
    crate::debug::emit_marker("SORT_PASS2_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "sort.pass2",
        "Pass 2: writing sorted output..."
    );
    crate::debug::emit_marker("SORT_WRITER_SETUP_START");
    // Sorted by construction. The way enrichments follow from the index pass
    // 1 built: a way blob in a rewrite run is decoded and re-encoded without
    // them, an unindexed passthrough way blob is reframed (field 5 dropped),
    // every other way blob is copied whole.
    let ways = super::output_header::Ways::from_copy(
        entries
            .iter()
            .zip(&overlaps)
            .any(|(e, &o)| o && e.index.kind == ElementKind::Way),
        entries
            .iter()
            .zip(&overlaps)
            .any(|(e, &o)| !o && !e.has_indexdata && e.index.kind == ElementKind::Way),
    );
    let header_bytes = super::output_header::build_one(
        &header,
        &super::output_header::OutputFacts::sorted(ways),
        overrides,
        host,
    )?
    .bytes;
    let mut writer = writer_from_header_bytes(output, compression, &header_bytes, backend, host)?;
    crate::debug::emit_marker("SORT_WRITER_SETUP_END");

    // Open input for random-access reads
    let mut input_file = File::open(input)?;

    // copy_file_range / CopyRange setup. The writer thread splices from this
    // handle asynchronously, so it gets its own `dup`ed descriptor owned by an
    // `Arc` that outlives every queued CopyRange - see
    // `PbfWriter::write_raw_copy`. `input_file` stays untouched for the
    // random-access reads below.
    let input_shared = std::sync::Arc::new(input_file.try_clone()?);
    // Buffered output splices with copy_file_range; the io_uring writer
    // services a CopyRange with a pread into a registered buffer followed by
    // a plain write (the linked ReadFixed+WriteFixed chain was removed).
    // O_DIRECT cannot take either.
    let use_copy_range = backend.supports_copy_range();

    let mut stats = SortStats {
        nodes: 0,
        ways: 0,
        relations: 0,
        passthrough_claimed: 0,
        blobs_passthrough: 0,
        blobs_rewritten: 0,
        blobs_total: entries.len() as u64,
    };

    let mut frame_buf: Vec<u8> = Vec::new();

    // Collect kind-bounded overlap-run spans upfront so they can be
    // processed in parallel before the serial write loop. `detect_overlaps`
    // marks `overlaps[i]` based on same-kind adjacency, but two same-kind
    // overlap-runs can sit adjacent in file order (e.g. a node/node
    // overlap pair immediately followed by a way/way overlap pair, both
    // with overlaps[i]=true). Splitting at kind boundaries mirrors the
    // `cat::dedupe::merge_pbf` bugfix in 486d4d1 - handing mixed-kind
    // entries to a kind-gated sweep would silently drop off-kind
    // elements.
    let overlap_runs: Vec<(usize, usize, ElementKind)> = collect_overlap_runs(&entries, &overlaps);
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("sort_overlap_runs", overlap_runs.len() as i64);

    // Parallel overlap rewrite: each rayon worker decodes its run, runs the
    // sweep-merge, and emits owned blocks into a local `Vec<OwnedBlock>`.
    // The main thread drains the results in input order below, interleaved
    // with passthrough CFR operations. Memory cost: buffered output for all
    // overlap runs. For typical "mostly sorted" input (few small runs)
    // this is small; for pathologically unsorted input it can approach the
    // input size.
    let overlap_outputs: Vec<(Vec<OwnedBlock>, OverlapCounts)> = if overlap_runs.is_empty() {
        Vec::new()
    } else {
        crate::debug::emit_marker("SORT_OVERLAP_PARALLEL_START");
        let results: crate::Result<Vec<_>> = host.executor.install(|| {
            overlap_runs
                .par_iter()
                .map(|(start, end, kind)| {
                    compute_overlap_run_local(&entries[*start..*end], *kind, input)
                })
                .collect()
        });
        let out = results?;
        crate::debug::emit_marker("SORT_OVERLAP_PARALLEL_END");
        out
    };

    crate::debug::emit_marker("SORT_WRITE_LOOP_START");
    // Passthrough coalescer: adjacent blobs contiguous in the input file
    // collapse into one `copy_file_range` call. On already-sorted input this
    // is the entire file, cutting syscall count from O(blobs) to O(1).
    // Mirrors the contiguous-range passthrough coalescer in apply_changes.
    let mut copy_run: Option<(u64, u64)> = None;
    let mut copy_run_calls: u64 = 0;
    let mut copy_run_coalesced: u64 = 0;
    let mut overlap_iter = overlap_outputs.into_iter();
    let mut i = 0;
    while i < entries.len() {
        if overlaps[i] {
            flush_copy_run(
                &mut copy_run,
                &mut writer,
                &input_shared,
                &mut copy_run_calls,
            )?;
            let start = i;
            let run_kind = entries[i].index.kind;
            while i < entries.len() && overlaps[i] && entries[i].index.kind == run_kind {
                i += 1;
            }
            let (blocks, counts) = overlap_iter
                .next()
                .ok_or_else(|| Error::internal("overlap output iterator drained"))?;
            for OwnedBlock {
                bytes: block_bytes,
                index,
                tagdata,
                way_members,
            } in blocks
            {
                writer.write_primitive_block_owned(
                    block_bytes,
                    index,
                    tagdata.as_deref(),
                    way_members.as_deref(),
                )?;
            }
            stats.nodes += counts.nodes;
            stats.ways += counts.ways;
            stats.relations += counts.relations;
            stats.blobs_rewritten += (i - start) as u64;
        } else {
            let entry = &entries[i];
            match try_extend_copy_run(
                &mut copy_run,
                entry,
                use_copy_range,
                &mut writer,
                &input_shared,
                &mut copy_run_calls,
            )? {
                CopyRunStep::Extended => copy_run_coalesced += 1,
                CopyRunStep::Started => {}
                CopyRunStep::Fallback => {
                    flush_copy_run(
                        &mut copy_run,
                        &mut writer,
                        &input_shared,
                        &mut copy_run_calls,
                    )?;
                    write_passthrough_blob(entry, &mut input_file, &mut writer, &mut frame_buf)?;
                }
            }
            count_entry(entry, &mut stats);
            stats.blobs_passthrough += 1;
            i += 1;
        }
    }
    flush_copy_run(
        &mut copy_run,
        &mut writer,
        &input_shared,
        &mut copy_run_calls,
    )?;
    debug_assert!(
        overlap_iter.next().is_none(),
        "overlap outputs not fully drained"
    );

    crate::debug::emit_marker("SORT_WRITE_LOOP_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("sort_blobs_passthrough", stats.blobs_passthrough as i64);
        crate::debug::emit_counter("sort_blobs_rewritten", stats.blobs_rewritten as i64);
        crate::debug::emit_counter("sort_copy_range_calls", copy_run_calls as i64);
        crate::debug::emit_counter("sort_copy_range_coalesced", copy_run_coalesced as i64);
    }

    crate::debug::emit_marker("SORT_FLUSH_START");
    // WAIT_WRITER spans the main thread's wait on the writer worker
    // to drain the coalesced `copy_file_range` (or its uring / EXDEV
    // fallback equivalent). On already-sorted input this is ~94 %
    // of wall, which `brokkr sidecar --durations` shows as the WAIT_WRITER
    // span (`--stalls` reads `*_wait_ns` counters, not marker pairs).
    crate::debug::emit_marker("WAIT_WRITER_START");
    writer.flush()?;
    crate::debug::emit_marker("WAIT_WRITER_END");
    crate::debug::emit_marker("SORT_FLUSH_END");
    #[allow(clippy::cast_possible_wrap)]
    if let Ok(meta) = std::fs::metadata(output.path()) {
        crate::debug::emit_counter("sort_total_bytes_out", meta.len() as i64);
    }
    crate::debug::emit_marker("SORT_PASS2_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Pass 1: Build blob index
// ---------------------------------------------------------------------------

/// Build a blob-level index of the input file.
///
/// Walks blob headers via `HeaderWalker` (pread-only, `fadvise(RANDOM)`).
/// When the input header claims `Sort.Type_then_ID`, blobs with indexdata
/// are classified without touching payload bytes. All other blobs -
/// non-indexed ones, and indexed ones of an input that does NOT declare
/// itself sorted - take the fallback path that preads + decompresses the
/// payload and scans element IDs, and while doing so checks intra-blob
/// monotonicity (flagging blobs whose elements are internally out of
/// canonical OSM ID order so pass 2 re-encodes them). Indexdata alone does
/// not prove intra-blob order: `cat` attaches indexdata to third-party
/// payloads it never reorders, and `PbfWriter::write_primitive_block`
/// indexes caller-provided blocks as-is; the trusted ordering signal is the
/// header claim, the same contract `ElementReader` keys its monotonicity
/// guarantees on (see CORRECTNESS.md). `direct_io` is accepted for signature
/// stability but unused here - the walker opens its own buffered fd.
/// Twin of the migration done for `inspect/scan.rs::try_index_only_scan`
/// (planet pass 1 was 21 s / 36 GB read through the buffered reader's
/// readahead; the walker's per-header preads avoid pulling payloads into
/// the page cache entirely).
#[hotpath::measure]
fn build_blob_index(
    input: &Path,
    _direct_io: bool,
    host: &crate::host::Host,
) -> Result<(crate::HeaderBlock, Vec<BlobEntry>)> {
    let mut walker = HeaderWalker::open(input, &host.diagnostics)?;
    let mut entries = Vec::new();
    let mut header: Option<crate::HeaderBlock> = None;
    let mut data_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut warned_unclaimed_indexed = false;

    while let Some(meta) = walker.next_header()? {
        match meta.blob_type {
            BlobKind::OsmHeader if header.is_none() => {
                walker
                    .pread_data(meta.data_offset, meta.data_size, &mut data_buf)
                    .map_err(|e| walker.locate_meta(&meta, e))?;
                header = Some(
                    decode_blob_to_headerblock(&data_buf)
                        .map_err(|e| walker.locate_meta(&meta, e))?,
                );
            }
            BlobKind::OsmData => {
                let has_indexdata = meta.index.is_some();
                // The header blob precedes all data blobs in a well-formed
                // PBF; if a data blob somehow arrives first, `header` is
                // still None and the blob conservatively takes the checked
                // scan below.
                let header_claims_sorted =
                    header.as_ref().is_some_and(crate::HeaderBlock::is_sorted);
                let mut index_scanned = true;
                let (index, intra_unsorted) = match meta.index.filter(|_| header_claims_sorted) {
                    // Declared-sorted input: pass 1 reads only the indexdata
                    // header and never touches the payload. Intra-blob order
                    // is a precondition of the Sort.Type_then_ID claim
                    // itself, upheld by pbfhogg's own sorted producers and
                    // trusted here exactly as ElementReader trusts it (see
                    // CORRECTNESS.md).
                    Some(idx) => {
                        index_scanned = false;
                        (idx, false)
                    }
                    None => {
                        // No indexdata, or indexdata on an input that does
                        // not declare itself sorted (e.g. unsorted input run
                        // through `cat`, which indexes blobs without
                        // reordering them) - pread payload, decompress, scan
                        // IDs with the intra-blob order check.
                        if has_indexdata && !warned_unclaimed_indexed {
                            warned_unclaimed_indexed = true;
                            crate::host::diag!(
                                host.diagnostics,
                                Note,
                                "sort.verify_unclaimed_order",
                                "input does not declare Sort.Type_then_ID; verifying \
                                 intra-blob order (pass 1 decodes every blob payload)"
                            );
                        }
                        scan_payload_checked(&walker, &meta, &mut data_buf, &mut decompress_buf)
                            .map_err(|e| walker.locate_meta(&meta, e))?
                    }
                };
                entries.push(BlobEntry {
                    file_offset: meta.frame_start,
                    frame_len: meta.frame_size as u64,
                    index,
                    has_indexdata,
                    tagdata: meta.tagdata,
                    intra_unsorted,
                    index_scanned,
                });
            }
            _ => {
                // Unknown extension blob: walker has already advanced past
                // the payload. It rejects non-leading OSMHeader frames.
            }
        }
    }

    let header = header.ok_or_else(|| crate::error::new_missing_header(input))?;

    Ok((header, entries))
}

/// Pass-1 payload fallback: pread + decompress one blob's payload and scan
/// element IDs, checking intra-blob monotonicity along the way. Returns the
/// freshly scanned `BlobIndex` (used for range analysis in preference to any
/// stored indexdata, which is unverified on these inputs) and whether the
/// blob is internally OUT of canonical OSM ID order.
///
/// Used for non-indexed blobs (no other way to learn the ID range) and for
/// indexed blobs of an input whose header does not claim
/// `Sort.Type_then_ID` (indexdata proves nothing about internal order -
/// see CORRECTNESS.md). The monotonicity check is one compare per element
/// on a scan that already visits every element ID, so effectively free
/// relative to the pread + decompress it rides on.
fn scan_payload_checked(
    walker: &HeaderWalker,
    meta: &crate::read::header_walker::BlobHeaderMeta,
    data_buf: &mut Vec<u8>,
    decompress_buf: &mut Vec<u8>,
) -> Result<(BlobIndex, bool)> {
    walker.pread_data(meta.data_offset, meta.data_size, data_buf)?;
    decompress_blob_data_into(data_buf, decompress_buf)?;
    let (index, sorted) = scan_block_ids_checked(decompress_buf)
        .ok_or_else(|| Error::invalid_data("failed to scan block IDs"))?;
    Ok((index, !sorted))
}

// ---------------------------------------------------------------------------
// Sort + overlap detection
// ---------------------------------------------------------------------------

fn type_order(kind: ElementKind) -> u8 {
    match kind {
        ElementKind::Node => 0,
        ElementKind::Way => 1,
        ElementKind::Relation => 2,
    }
}

/// Detect overlapping blob runs in sorted entries.
///
/// Two adjacent blobs of the same type overlap if the first's max_id >=
/// the second's min_id. Returns a boolean vec where `true` marks entries
/// that must be decoded and re-encoded.
/// Fold intra-blob disorder flags into the overlap set, returning the number
/// of blobs newly routed to rewrite.
///
/// `entry.intra_unsorted` is only ever set by the pass-1 payload scan, which
/// runs for non-indexed blobs and for indexed blobs of an input that does
/// not declare `Sort.Type_then_ID` (declared-sorted indexed blobs skip the
/// payload and are trusted - see the `BlobEntry` field doc and
/// CORRECTNESS.md). Marking such a blob as an overlap makes pass 2 decode and
/// re-encode it through the sweep-merge, repairing the internal order even
/// when its ID range does not overlap a neighbour.
///
/// The returned count is the number of blobs NEWLY routed to rewrite by this
/// pass - blobs not already flagged by `detect_overlaps`. A blob that is both
/// intra-unsorted AND genuinely range-overlapping is already in the rewrite
/// set and is not counted here, keeping this figure disjoint from the
/// genuine-overlap count the caller captured beforehand.
fn mark_intra_unsorted_for_rewrite(entries: &[BlobEntry], overlaps: &mut [bool]) -> u64 {
    let mut count = 0;
    for (i, entry) in entries.iter().enumerate() {
        if entry.intra_unsorted && !overlaps[i] {
            overlaps[i] = true;
            count += 1;
        }
    }
    count
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

/// Fallback passthrough path: buffered read + reframe (when indexdata is
/// missing) or raw `write_raw` (when present but `copy_file_range` isn't
/// usable for this blob - e.g. O_DIRECT output).
#[hotpath::measure]
fn write_passthrough_blob(
    entry: &BlobEntry,
    input_file: &mut File,
    writer: &mut PbfWriter<FileWriter>,
    frame_buf: &mut Vec<u8>,
) -> Result<()> {
    if entry.has_indexdata {
        read_frame_into(input_file, entry, frame_buf)?;
        writer.write_raw(frame_buf)?;
    } else {
        // Reframe with indexdata before writing.
        read_frame_into(input_file, entry, frame_buf)?;
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

/// Outcome of offering a blob to the `copy_file_range` coalescer.
#[allow(dead_code)] // `Extended`/`Started` only constructed under `linux-direct-io`.
enum CopyRunStep {
    /// Blob extended an in-flight run (contiguous with the current tail).
    Extended,
    /// Blob started a new run; any prior run was flushed.
    Started,
    /// Blob can't go through the copy path - missing indexdata or
    /// `copy_file_range` unavailable. Caller flushes + uses the fallback.
    Fallback,
}

/// Try to fold `entry` into an in-flight `copy_file_range` run.
#[allow(unused_variables)]
fn try_extend_copy_run(
    run: &mut Option<(u64, u64)>,
    entry: &BlobEntry,
    use_copy_range: bool,
    writer: &mut PbfWriter<FileWriter>,
    input: &std::sync::Arc<std::fs::File>,
    calls: &mut u64,
) -> Result<CopyRunStep> {
    if !(entry.has_indexdata && use_copy_range) {
        return Ok(CopyRunStep::Fallback);
    }
    #[cfg(feature = "linux-direct-io")]
    {
        match *run {
            Some((start, end)) if end == entry.file_offset => {
                *run = Some((start, end + entry.frame_len));
                Ok(CopyRunStep::Extended)
            }
            _ => {
                flush_copy_run(run, writer, input, calls)?;
                *run = Some((entry.file_offset, entry.file_offset + entry.frame_len));
                Ok(CopyRunStep::Started)
            }
        }
    }
    #[cfg(not(feature = "linux-direct-io"))]
    Ok(CopyRunStep::Fallback)
}

/// Emit any in-flight `copy_file_range` run as a single `write_raw_copy`
/// call and clear the run state. No-op when no run is in flight.
#[cfg(feature = "linux-direct-io")]
fn flush_copy_run(
    run: &mut Option<(u64, u64)>,
    writer: &mut PbfWriter<FileWriter>,
    input: &std::sync::Arc<std::fs::File>,
    calls: &mut u64,
) -> Result<()> {
    if let Some((start, end)) = run.take() {
        writer.write_raw_copy(input, start, end - start)?;
        *calls += 1;
    }
    Ok(())
}

/// No-op stub: without `linux-direct-io`, `try_extend_copy_run` never
/// populates `run`, so this only ever sees `None`.
#[cfg(not(feature = "linux-direct-io"))]
fn flush_copy_run(
    run: &mut Option<(u64, u64)>,
    _writer: &mut PbfWriter<FileWriter>,
    _input: &std::sync::Arc<std::fs::File>,
    _calls: &mut u64,
) -> Result<()> {
    debug_assert!(
        run.is_none(),
        "copy_file_range run set without linux-direct-io feature"
    );
    run.take();
    Ok(())
}

/// Read a complete frame from the input file at the given offset into `buf`.
fn read_frame_into(file: &mut File, entry: &BlobEntry, buf: &mut Vec<u8>) -> io::Result<()> {
    file.seek(SeekFrom::Start(entry.file_offset))?;
    #[allow(clippy::cast_possible_truncation)]
    let len = entry.frame_len as usize;
    buf.clear();
    buf.resize(len, 0);
    file.read_exact(buf)?;
    Ok(())
}

/// Extract the raw Blob bytes from a complete frame.
///
/// Frame layout: `[4-byte header_len][BlobHeader][Blob]`.
fn extract_blob_bytes(frame: &[u8]) -> Result<&[u8]> {
    if frame.len() < 4 {
        return Err(Error::invalid_data("frame too short"));
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let blob_start = 4 + header_len;
    if blob_start > frame.len() {
        return Err(Error::invalid_data("invalid header length"));
    }
    Ok(&frame[blob_start..])
}

/// Add element counts from a blob entry to stats.
fn count_entry(entry: &BlobEntry, stats: &mut SortStats) {
    if !entry.index_scanned {
        // Declared-sorted fast path: the payload was never touched, so the
        // only count available is the one the header claims. Keep it out of
        // the observed totals.
        stats.passthrough_claimed += entry.index.count;
        return;
    }
    match entry.index.kind {
        ElementKind::Node => stats.nodes += entry.index.count,
        ElementKind::Way => stats.ways += entry.index.count,
        ElementKind::Relation => stats.relations += entry.index.count,
    }
}

// ---------------------------------------------------------------------------
// Pass 2: Parallel overlap run rewrite
// ---------------------------------------------------------------------------

/// Per-overlap-run stats, merged into the main `SortStats` by the write loop.
#[derive(Default)]
struct OverlapCounts {
    nodes: u64,
    ways: u64,
    relations: u64,
}

/// Collect kind-bounded overlap-run spans from `entries` + `overlaps`.
///
/// Each returned `(start, end, kind)` describes a contiguous slice of
/// same-kind overlap entries. Kind boundaries split runs even when
/// `overlaps[i]` is set across them - a node/node overlap-pair followed
/// by a way/way overlap-pair must stay in separate runs, otherwise a
/// kind-gated sweep handed the combined slice would silently drop
/// off-kind elements (same pattern as `cat::dedupe::merge_pbf` in
/// 486d4d1).
fn collect_overlap_runs(
    entries: &[BlobEntry],
    overlaps: &[bool],
) -> Vec<(usize, usize, ElementKind)> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        if overlaps[i] {
            let start = i;
            let run_kind = entries[i].index.kind;
            while i < entries.len() && overlaps[i] && entries[i].index.kind == run_kind {
                i += 1;
            }
            runs.push((start, i, run_kind));
        } else {
            i += 1;
        }
    }
    runs
}

/// Decode one overlap run on a rayon worker and emit owned blocks into a
/// local `Vec<OwnedBlock>`. Each worker opens its own input fd so there
/// is no shared reader state; results are collected by the main thread
/// and drained in input order.
fn compute_overlap_run_local(
    entries: &[BlobEntry],
    kind: ElementKind,
    input_path: &Path,
) -> crate::Result<(Vec<OwnedBlock>, OverlapCounts)> {
    let mut input_file =
        File::open(input_path).map_err(|e| crate::file_reader::open_error(input_path, e))?;
    let mut bb = BlockBuilder::new();
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut counts = OverlapCounts::default();
    match kind {
        ElementKind::Node => {
            counts.nodes = sweep_merge_local(
                entries,
                &mut input_file,
                &mut bb,
                &mut output,
                |e, heap| match e {
                    Element::DenseNode(dn) => heap.push(Reverse(read_dense_node(dn))),
                    Element::Node(n) => heap.push(Reverse(read_node(n))),
                    _ => {}
                },
                write_single_node_local,
                input_path,
            )?;
        }
        ElementKind::Way => {
            counts.ways = sweep_merge_local(
                entries,
                &mut input_file,
                &mut bb,
                &mut output,
                |e, heap| {
                    if let Element::Way(w) = e {
                        heap.push(Reverse(read_way(w)));
                    }
                },
                write_single_way_local,
                input_path,
            )?;
        }
        ElementKind::Relation => {
            counts.relations = sweep_merge_local(
                entries,
                &mut input_file,
                &mut bb,
                &mut output,
                |e, heap| {
                    if let Element::Relation(r) = e {
                        heap.push(Reverse(read_relation(r)));
                    }
                },
                write_single_relation_local,
                input_path,
            )?;
        }
    };
    Ok((output, counts))
}

// ---------------------------------------------------------------------------
// Local sweep merge (rayon-worker-friendly)
// ---------------------------------------------------------------------------

fn sweep_merge_local<T: Ord + HasId>(
    entries: &[BlobEntry],
    input_file: &mut File,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    mut extract: impl FnMut(&Element<'_>, &mut BinaryHeap<Reverse<T>>),
    mut write_elem: impl FnMut(&T, &mut BlockBuilder, &mut Vec<OwnedBlock>) -> crate::Result<()>,
    input_path: &Path,
) -> crate::Result<u64> {
    let mut heap: BinaryHeap<Reverse<T>> = BinaryHeap::new();
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut count: u64 = 0;

    for entry in entries {
        flush_heap_below_local(
            &mut heap,
            crate::osm_id::blob_osm_first_id(entry.index.min_id, entry.index.max_id),
            |elem| {
                write_elem(&elem, bb, output)?;
                count += 1;
                Ok(())
            },
        )?;

        // The run's blobs are addressed by frame offset in `input_path`; a
        // failure on one is located there (the worker's input is captured
        // once, in `input_path`).
        let locate = |e: Error| e.locate(frame_location(input_path, entry.file_offset));
        read_frame_into(input_file, entry, &mut frame_buf).map_err(|e| locate(e.into()))?;
        let blob_bytes = extract_blob_bytes(&frame_buf).map_err(locate)?;
        let block = decode_blob_to_primitiveblock(blob_bytes).map_err(locate)?;
        for element in block.elements() {
            extract(&element, &mut heap);
        }
    }

    while let Some(Reverse(elem)) = heap.pop() {
        write_elem(&elem, bb, output)?;
        count += 1;
    }
    crate::commands::flush_local(bb, output)?;
    Ok(count)
}

fn flush_heap_below_local<T: Ord + HasId>(
    heap: &mut BinaryHeap<Reverse<T>>,
    below: i64,
    mut emit: impl FnMut(T) -> crate::Result<()>,
) -> crate::Result<()> {
    while heap
        .peek()
        .is_some_and(|Reverse(e)| crate::osm_id::osm_id_cmp(e.id(), below).is_lt())
    {
        if let Some(Reverse(element)) = heap.pop() {
            emit(element)?;
        }
    }
    Ok(())
}

/// Trait for accessing the ID of owned element types.
trait HasId {
    fn id(&self) -> i64;
}

impl HasId for OwnedNode {
    fn id(&self) -> i64 {
        self.id
    }
}

impl HasId for OwnedWay {
    fn id(&self) -> i64 {
        self.id
    }
}

impl HasId for OwnedRelation {
    fn id(&self) -> i64 {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ElementKind, min_id: i64, max_id: i64) -> BlobEntry {
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
            intra_unsorted: false,
            index_scanned: true,
        }
    }

    /// The transitivity failure. Entries are in first-key order:
    /// A=[1..1000], B=[2..3], C=[500..600].
    ///
    /// A/B overlap. B/C do NOT (`B.max=3 < C.min=500`), so a pairwise-adjacent
    /// scan leaves C unflagged - yet C's range sits squarely inside A's. The
    /// run [A,B] then flushes A's ids 500..1000 and C is written afterwards as
    /// a raw passthrough blob carrying ids 500..600, so the output is not
    /// sorted while the header unconditionally stamps `Sort.Type_then_ID`.
    #[test]
    fn overlap_runs_are_transitive_not_pairwise() {
        let entries = vec![
            entry(ElementKind::Node, 1, 1000),
            entry(ElementKind::Node, 2, 3),
            entry(ElementKind::Node, 500, 600),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, true],
            "C=[500..600] lies inside A=[1..1000] and must join the run even \
             though it does not overlap its immediate predecessor B=[2..3]"
        );
    }

    /// Passing counterpart: genuinely disjoint blobs must stay unflagged, so a
    /// "flag everything" regression cannot satisfy the test above.
    #[test]
    fn disjoint_blobs_are_not_flagged_as_overlapping() {
        let entries = vec![
            entry(ElementKind::Node, 1, 10),
            entry(ElementKind::Node, 11, 20),
            entry(ElementKind::Node, 21, 30),
        ];
        assert_eq!(detect_overlaps(&entries), vec![false, false, false]);
    }

    /// Runs stay kind-bounded: the sweep-merge is kind-gated and silently
    /// drops off-kind elements handed to it.
    #[test]
    fn a_run_never_crosses_a_kind_boundary() {
        let entries = vec![
            entry(ElementKind::Node, 1, 1000),
            entry(ElementKind::Node, 2, 3),
            entry(ElementKind::Way, 5, 6),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, false],
            "the way blob shares an id range with the node run but is a \
             different kind, so it must not be pulled into it"
        );
    }

    /// A run that ends must not tint the blob that follows it.
    #[test]
    fn a_new_run_starts_after_the_previous_one_ends() {
        let entries = vec![
            entry(ElementKind::Node, 1, 100),
            entry(ElementKind::Node, 50, 60),
            entry(ElementKind::Node, 200, 300),
            entry(ElementKind::Node, 250, 260),
        ];
        assert_eq!(detect_overlaps(&entries), vec![true, true, true, true]);

        let entries = vec![
            entry(ElementKind::Node, 1, 100),
            entry(ElementKind::Node, 50, 60),
            entry(ElementKind::Node, 200, 300),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, false],
            "the blob after a run must be judged on its own, not inherit the \
             run's flag"
        );
    }
}
