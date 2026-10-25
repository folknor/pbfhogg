//! Merge multiple sorted PBF files into one. Equivalent to `osmium merge`.
//!
//! Two-pass blob-level merge:
//!   Pass 1: Build blob index from all input files
//!   Pass 2: Write in sorted order - passthrough for non-overlapping blobs,
//!           decode + sweep merge with dedup for overlapping blobs.
//!
//! Exact duplicates (same type, ID, version) across input files are removed.
//! Common when merging geographic extracts that share border elements. Only
//! blobs whose ID ranges overlap are ever decoded, so duplicates *within* one
//! blob that overlaps nothing pass through untouched.
//!
//! The output header is reconciled across every input's header by the same
//! owner every command uses (`commands::output_header::build`).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::Element;
use crate::ElementKind;
use crate::blob::{
    BlobKind, decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
};
use crate::blob_meta::{BlobIndex, scan_block_ids};
use crate::block_builder::BlockBuilder;
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::frame::read_frame_header;
use crate::write::WriteOptions;
use crate::writer::{PbfWriter, reframe_raw_with_index};

use crate::commands::{
    HeaderOverrides, flush_block, require_indexdata, require_sorted, writer_from_header_bytes,
};
use crate::error::{Error, Result};
use crate::owned::{
    OwnedNode, OwnedRelation, OwnedWay, read_dense_node, read_node, read_relation, read_way,
    write_single_node, write_single_relation, write_single_way,
};
use crate::read::raw_frame::frame_location;

/// Statistics from a multi-PBF merge operation.
///
/// `nodes`, `ways` and `relations` are *observed* output counts: elements the
/// merge decoded and emitted from an overlap run (duplicates it dropped are in
/// `duplicates_removed`, not here), plus passthrough blobs without indexdata,
/// whose count came from decompressing and scanning the payload.
pub struct MergePbfStats {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    /// Elements in passthrough blobs whose count is the blob's own
    /// `BlobHeader.indexdata` claim, copied through without a decode.
    ///
    /// Kept apart from the observed counts on purpose, as in `sort` and
    /// `getid`: nothing verifies that claim (`check --index` exists to), so
    /// folding it into "Merged N elements" reported hearsay as observation and
    /// a lying indexdata moved N with no way for the reader to tell.
    pub passthrough_claimed: u64,
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
        if self.passthrough_claimed > 0 {
            writeln!(
                w,
                "  plus {} elements in blobs copied through without decoding, per \
                 their indexdata (unverified)",
                self.passthrough_claimed,
            )?;
        }

        Ok(())
    }
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

/// Merge multiple sorted PBF files into one, removing exact duplicates
/// (same type, ID and version) across inputs.
///
/// All inputs must be sorted (Sort.Type_then_ID), and each must start with
/// exactly one OSMHeader blob. Uses blob-level index for fast passthrough of
/// non-overlapping blobs; overlapping blobs are decoded, merged, and deduped
/// via a streaming sweep. A blob that overlaps no other is never decoded, so
/// duplicates inside it survive.
pub fn merge_pbf(
    inputs: &[&Path],
    output: crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergePbfStats> {
    host.cancel.check()?;
    let result = merge_pbf_to(inputs, &output, opts, overrides, host)?;
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`merge_pbf`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn merge_pbf_to(
    inputs: &[&Path],
    output: &crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergePbfStats> {
    let plan = plan_merge(inputs, opts, host)?;
    write_merge(plan, output, opts, overrides, host)
}

/// What pass 1 learned, and everything pass 2 needs from it. The inputs
/// travel with the entries, so an entry's `file_index` can only ever be
/// resolved against the list it was indexed from.
struct MergePlan {
    inputs: Vec<std::path::PathBuf>,
    headers: Vec<crate::HeaderBlock>,
    /// Every OSMData blob of every input, in (type, first ID) order.
    entries: Vec<BlobEntry>,
    /// Per entry: whether it belongs to an overlap run (decode, merge,
    /// dedupe) rather than passing through.
    overlaps: Vec<bool>,
}

/// Pass 1: check the inputs, index every blob, order the index and find the
/// overlap runs.
fn plan_merge(
    inputs: &[&Path],
    opts: &WriteOptions,
    host: &crate::host::Host,
) -> Result<MergePlan> {
    if inputs.is_empty() {
        return Err(Error::invalid_argument("no input files specified"));
    }

    // Validate inputs have indexdata
    for input in inputs {
        require_indexdata(
            *input,
            opts.direct_reads,
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
    let (headers, mut entries) = build_blob_index(inputs, opts.direct_reads, host)?;
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

    Ok(MergePlan {
        inputs: inputs.iter().map(|p| p.to_path_buf()).collect(),
        headers,
        entries,
        overlaps,
    })
}

/// Pass 2: write the planned blobs in order, re-reading each frame from the
/// input pass 1 found it in. A frame that no longer reads or decodes (the
/// file changed between passes) fails located at that input and the frame's
/// offset.
fn write_merge(
    plan: MergePlan,
    output: &crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergePbfStats> {
    let MergePlan {
        inputs,
        headers,
        entries,
        overlaps,
    } = plan;

    // Pass 2: Write in sorted order
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.pass2",
        "Pass 2: writing merged output..."
    );
    // Reconciled across every input, not taken from the first: the merge's
    // header has to describe the merge. Always sorted - every input passed
    // `require_sorted` and the sweep emits in type-then-ID order. The way
    // enrichments follow from the index just built: a way blob in an overlap
    // run is re-encoded, an unindexed passthrough way blob is reframed.
    let ways = crate::commands::output_header::Ways::from_copy(
        entries
            .iter()
            .zip(&overlaps)
            .any(|(e, &o)| o && e.index.kind == ElementKind::Way),
        entries
            .iter()
            .zip(&overlaps)
            .any(|(e, &o)| !o && !e.has_indexdata && e.index.kind == ElementKind::Way),
    );
    let header = crate::commands::output_header::build(
        &headers,
        &crate::commands::output_header::OutputFacts::sorted(ways),
        overrides,
        host,
    )?;
    let mut writer =
        writer_from_header_bytes(output, opts.compression, &header.bytes, opts.backend, host)?;

    let mut files = open_sources(&inputs)?;

    let mut stats = MergePbfStats {
        nodes: 0,
        ways: 0,
        relations: 0,
        passthrough_claimed: 0,
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

/// Index every OSMData blob of every input, and collect each input's header.
///
/// Returns one header per input, in input order. Each input must carry
/// exactly one OSMHeader, as its first blob (libosmium's
/// `pbf_input_format` enforces the same). The per-input rule is what makes
/// `require_sorted` cover every input: when only the first header found
/// across all inputs was kept, an input with no OSMHeader at all never met
/// the sortedness check, so an unsorted headerless input was merged under a
/// `Sort.Type_then_ID` stamp as long as some other input had a header. A
/// second header later in a file is refused by the shared framing policy
/// (`read::frame::check_header_position`) rather than ignored, because
/// whatever it declares (history, a bbox, enrichment) would otherwise be
/// silently left out of the reconciled output header.
fn build_blob_index(
    inputs: &[&Path],
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<(Vec<crate::HeaderBlock>, Vec<BlobEntry>)> {
    let mut entries = Vec::new();
    let mut headers: Vec<crate::HeaderBlock> = Vec::with_capacity(inputs.len());
    let mut scratch = IndexScratch::default();

    for (file_idx, input) in inputs.iter().enumerate() {
        headers.push(index_input(
            file_idx,
            input,
            direct_io,
            host,
            &mut scratch,
            &mut entries,
        )?);
    }

    Ok((headers, entries))
}

/// Buffers reused across every frame of every input in pass 1.
#[derive(Default)]
struct IndexScratch {
    decompress_buf: Vec<u8>,
    header_buf: Vec<u8>,
    blob_buf: Vec<u8>,
}

/// Pass 1 over one input: append its OSMData blobs to `entries` and return
/// its header. See [`build_blob_index`] for the header rules.
fn index_input(
    file_idx: usize,
    input: &Path,
    direct_io: bool,
    host: &crate::host::Host,
    scratch: &mut IndexScratch,
    entries: &mut Vec<BlobEntry>,
) -> Result<crate::HeaderBlock> {
    let IndexScratch {
        decompress_buf,
        header_buf,
        blob_buf,
    } = scratch;
    let mut reader = FileReader::open(input, direct_io)?;
    let mut file_offset: u64 = 0;
    let mut header: Option<crate::HeaderBlock> = None;

    loop {
        // Once per frame: this walk reads its own frames rather than going
        // through a cancellation-aware reader.
        host.cancel.check()?;
        let frame_start = file_offset;
        // This loop walks the input, so it owns the provenance of every
        // failure in the frame it is on.
        let locate = |e: Error| e.locate(frame_location(input, frame_start));

        // Prefix, EOF policy, the zero-length and MAX_BLOB_HEADER_SIZE guards,
        // and the header parse all come from `read::frame`. This loop
        // previously resized `header_buf` straight from the unvalidated 4-byte
        // prefix, so four bytes of corrupt input asked for up to a 4 GiB
        // allocation - the one frame decoder that had neither guard.
        // Name the input: this loop indexes several files, and the shared
        // framing refusals (a non-leading OSMHeader among them, which this
        // loop used to report itself with the input path) carry none.
        let Some(frame) = read_frame_header(
            &mut reader,
            header_buf,
            frame_start,
            &host.diagnostics,
            Some(input),
        )
        .map_err(locate)?
        else {
            break;
        };
        let raw_index = frame
            .raw_index
            .as_ref()
            .and_then(|d| BlobIndex::deserialize(d));
        let frame_len = (frame.payload_offset() + frame.data_size) as u64;
        let data_size = frame.data_size;
        file_offset += frame_len;

        match (&frame.blob_type, header.is_some()) {
            (BlobKind::OsmHeader, _) => {
                blob_buf.resize(data_size, 0);
                reader.read_exact(blob_buf).map_err(|e| locate(e.into()))?;
                let hdr = decode_blob_to_headerblock(blob_buf).map_err(locate)?;
                require_sorted(
                    hdr.is_sorted(),
                    input,
                    &format!("Input file {}", input.display()),
                    "Cat dedupe requires sorted inputs so it can merge them in type-then-ID order.",
                )
                .map_err(locate)?;
                header = Some(hdr);
            }
            (_, false) => {
                return Err(crate::error::new_missing_header(input));
            }
            (BlobKind::OsmData, true) => {
                let has_indexdata = raw_index.is_some();
                let index = if let Some(index) = raw_index {
                    reader
                        .skip(data_size as u64)
                        .map_err(|e| locate(e.into()))?;
                    index
                } else {
                    blob_buf.resize(data_size, 0);
                    reader.read_exact(blob_buf).map_err(|e| locate(e.into()))?;
                    decompress_blob_data_into(blob_buf, decompress_buf).map_err(locate)?;
                    scan_block_ids(decompress_buf)
                        .ok_or_else(|| locate(Error::invalid_data("failed to scan block IDs")))?
                };
                entries.push(BlobEntry {
                    file_offset: frame_start,
                    frame_len,
                    index,
                    has_indexdata,
                    tagdata: frame.tagdata,
                    file_index: file_idx,
                });
            }
            (_, true) => {
                reader
                    .skip(data_size as u64)
                    .map_err(|e| locate(e.into()))?;
            }
        }
    }

    header.ok_or_else(|| crate::error::new_missing_header(input))
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

/// One input of the second pass: its open file and the path it came from.
struct Source<'a> {
    file: File,
    path: &'a Path,
}

/// Open every input for pass 2's random access, each with its path so a
/// failed read or decode in this pass names the file.
fn open_sources(inputs: &[std::path::PathBuf]) -> Result<Vec<Source<'_>>> {
    inputs
        .iter()
        .map(|path| {
            Ok(Source {
                file: crate::read::input::open(path)?,
                path: path.as_path(),
            })
        })
        .collect()
}

impl Source<'_> {
    /// Locate a failure on `entry`'s frame in this input. Cold: built only
    /// on the error path.
    #[cold]
    fn locate(&self, entry: &BlobEntry, err: Error) -> Error {
        err.locate_blob(crate::error::Location::at_offset(
            self.path,
            entry.file_offset,
        ))
    }
}

fn write_passthrough_blob(
    entry: &BlobEntry,
    source: &mut Source<'_>,
    writer: &mut PbfWriter<FileWriter>,
    frame_buf: &mut Vec<u8>,
) -> Result<()> {
    read_frame_into(source, entry, frame_buf)?;
    if entry.has_indexdata {
        writer.write_raw(frame_buf)?;
    } else {
        let blob_bytes = extract_blob_bytes(frame_buf).map_err(|e| source.locate(entry, e))?;
        let reframed = reframe_raw_with_index(
            blob_bytes,
            &entry.index.serialize(),
            entry.tagdata.as_deref(),
        )
        .map_err(|e| source.locate(entry, e))?;
        writer.write_raw(&reframed)?;
    }
    Ok(())
}

fn read_frame_into(source: &mut Source<'_>, entry: &BlobEntry, buf: &mut Vec<u8>) -> Result<()> {
    #[allow(clippy::cast_possible_truncation)]
    let len = entry.frame_len as usize;
    buf.clear();
    buf.resize(len, 0);
    source
        .file
        .seek(SeekFrom::Start(entry.file_offset))
        .and_then(|_| source.file.read_exact(buf))
        .map_err(|e| source.locate(entry, Error::from(e)))
}

/// Re-read and decode the frame of an overlap-run `entry`, locating a read
/// or decode failure at its input and frame offset.
fn read_block(
    source: &mut Source<'_>,
    entry: &BlobEntry,
    buf: &mut Vec<u8>,
) -> Result<crate::PrimitiveBlock> {
    read_frame_into(source, entry, buf)?;
    extract_blob_bytes(buf)
        .and_then(decode_blob_to_primitiveblock)
        .map_err(|e| source.locate(entry, e))
}

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

/// Count a passthrough blob. Its payload was never decoded in pass 2, so the
/// only count available is the index's - observed when pass 1 built that
/// index by scanning the decompressed payload, a claim when it came from the
/// file's own `BlobHeader.indexdata`.
fn count_entry(entry: &BlobEntry, stats: &mut MergePbfStats) {
    if entry.has_indexdata {
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
// Pass 2: Overlap run rewrite with dedup
// ---------------------------------------------------------------------------

fn write_overlap_run(
    entries: &[BlobEntry],
    files: &mut [Source<'_>],
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergePbfStats,
    host: &crate::host::Host,
) -> Result<()> {
    let kind = entries[0].index.kind;
    match kind {
        ElementKind::Node => {
            let (c, d) = sweep_merge_dedup(
                entries,
                files,
                bb,
                writer,
                |e, heap| {
                    match e {
                        Element::DenseNode(dn) => heap.push(Reverse(read_dense_node(dn)?)),
                        Element::Node(n) => heap.push(Reverse(read_node(n)?)),
                        _ => {}
                    }
                    Ok(())
                },
                write_single_node,
                host,
            )?;
            stats.nodes += c;
            stats.duplicates_removed += d;
        }
        ElementKind::Way => {
            let (c, d) = sweep_merge_dedup(
                entries,
                files,
                bb,
                writer,
                |e, heap| {
                    if let Element::Way(w) = e {
                        heap.push(Reverse(read_way(w)?));
                    }
                    Ok(())
                },
                write_single_way,
                host,
            )?;
            stats.ways += c;
            stats.duplicates_removed += d;
        }
        ElementKind::Relation => {
            let (c, d) = sweep_merge_dedup(
                entries,
                files,
                bb,
                writer,
                |e, heap| {
                    if let Element::Relation(r) = e {
                        heap.push(Reverse(read_relation(r)?));
                    }
                    Ok(())
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
    files: &mut [Source<'_>],
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    mut extract: impl FnMut(&Element<'_>, &mut BinaryHeap<Reverse<T>>) -> Result<()>,
    mut write_elem: impl FnMut(&T, &mut BlockBuilder, &mut PbfWriter<FileWriter>) -> Result<()>,
    host: &crate::host::Host,
) -> Result<(u64, u64)> {
    let mut heap: BinaryHeap<Reverse<T>> = BinaryHeap::new();
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut count: u64 = 0;
    let mut deduped: u64 = 0;

    for entry in entries {
        // Once per blob of the run: a run whose heap only grows writes
        // nothing (so the writer's own check never runs) until it drains.
        host.cancel.check()?;
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

        let source = &mut files[entry.file_index];
        let block = read_block(source, entry, &mut frame_buf)?;
        for element in block.elements() {
            extract(&element, &mut heap).map_err(|e| source.locate(entry, e))?;
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

    /// Passing counterpart: disjoint blobs must stay on the passthrough path,
    /// which is the whole point of the blob-level merge.
    #[test]
    fn disjoint_blobs_are_not_flagged_as_overlapping() {
        let entries = vec![
            entry(ElementKind::Node, 1, 10),
            entry(ElementKind::Node, 11, 20),
            entry(ElementKind::Node, 21, 30),
        ];
        assert_eq!(detect_overlaps(&entries), vec![false, false, false]);
    }

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
            "handing a mixed-kind run to the kind-gated sweep silently drops \
             the off-kind elements"
        );
    }

    /// A passthrough blob's count is a claim when it came from indexdata and
    /// an observation when pass 1 scanned the payload. Only the latter may
    /// reach the observed per-kind totals.
    #[test]
    fn passthrough_counts_split_claimed_from_scanned() {
        let mut stats = MergePbfStats {
            nodes: 0,
            ways: 0,
            relations: 0,
            passthrough_claimed: 0,
            blobs_passthrough: 0,
            blobs_rewritten: 0,
            blobs_total: 0,
            duplicates_removed: 0,
        };
        let mut claimed = entry(ElementKind::Node, 1, 10);
        claimed.index.count = 1_000_000;
        count_entry(&claimed, &mut stats);
        assert_eq!(stats.nodes, 0, "an indexdata claim is not an observation");
        assert_eq!(stats.passthrough_claimed, 1_000_000);

        let mut scanned = entry(ElementKind::Way, 1, 10);
        scanned.has_indexdata = false;
        scanned.index.count = 7;
        count_entry(&scanned, &mut stats);
        assert_eq!(stats.ways, 7, "a pass-1 scan count is observed");
        assert_eq!(stats.passthrough_claimed, 1_000_000);
    }

    /// Kind of the one data blob a fixture writes into its own block.
    enum Blob<'a> {
        Nodes(&'a [i64]),
        Way(i64),
        Relation(i64),
    }

    /// A sorted, indexed PBF holding `blobs`, one block each, in order.
    fn write_fixture(path: &Path, blobs: &[Blob<'_>]) {
        use crate::block_builder::HeaderBuilder;
        use crate::writer::Compression;
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
            .expect("write header");
        let none = std::iter::empty::<(&str, &str)>;
        let mut bb = BlockBuilder::new();
        for blob in blobs {
            match blob {
                Blob::Nodes(ids) => {
                    for &id in *ids {
                        bb.add_node(id, 0, 0, none(), None);
                    }
                }
                Blob::Way(id) => bb.add_way(*id, none(), &[1, 2], None),
                Blob::Relation(id) => bb.add_relation(*id, none(), &[], None),
            }
            let bytes = bb.take().expect("encode").expect("non-empty block");
            writer.write_primitive_block(bytes).expect("write block");
        }
        writer.flush().expect("flush fixture");
    }

    /// Input A: nodes 1..=3 and a way, input B: nodes 2..=4 (overlapping
    /// A's node blob) and a relation that overlaps nothing.
    fn fixtures(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let a = dir.join("a.osm.pbf");
        let b = dir.join("b.osm.pbf");
        write_fixture(&a, &[Blob::Nodes(&[1, 2, 3]), Blob::Way(10)]);
        write_fixture(&b, &[Blob::Nodes(&[2, 3, 4]), Blob::Relation(100)]);
        (a, b)
    }

    fn plan(a: &Path, b: &Path) -> MergePlan {
        plan_merge(
            &[a, b],
            &WriteOptions::default(),
            &crate::host::Host::for_tests(),
        )
        .expect("pass 1")
    }

    fn run_pass2(dir: &Path, plan: MergePlan) -> Result<MergePbfStats> {
        let output = crate::output::Output::new(
            dir.join("merged.osm.pbf"),
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )?;
        write_merge(
            plan,
            &output,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &crate::host::Host::for_tests(),
        )
    }

    /// The entry of `kind` that pass 1 indexed from input `file_index`, and
    /// whether it is in an overlap run.
    fn entry_of(plan: &MergePlan, kind: ElementKind, file_index: usize) -> (&BlobEntry, bool) {
        let (entry, &overlap) = plan
            .entries
            .iter()
            .zip(&plan.overlaps)
            .find(|(e, _)| e.index.kind == kind && e.file_index == file_index)
            .expect("planned entry");
        (entry, overlap)
    }

    /// The untouched plan merges: the two shared nodes are deduped.
    #[test]
    fn pass2_merges_an_unchanged_plan() {
        let dir = crate::path_guard::tests::scratch();
        let (a, b) = fixtures(dir.path());
        let stats = run_pass2(dir.path(), plan(&a, &b)).expect("pass 2");
        assert_eq!(stats.nodes, 4);
        assert_eq!(stats.duplicates_removed, 2);
        assert_eq!(stats.blobs_rewritten, 2);
        assert_eq!(stats.blobs_passthrough, 2);
    }

    // The two failure cases drive pass 2's frame reads (`read_block` for an
    // overlap run, `read_frame_into` for a passthrough blob) over a real
    // pass-1 plan rather than `write_merge` itself: an error return drops
    // pass 2's `PbfWriter` unflushed, which this crate's unit-test builds
    // assert against (see the writer's `Drop`).

    /// A blob of an overlap run that no longer decodes when pass 2 re-reads
    /// it fails located at its input and its frame's offset - the second
    /// input, at a nonzero offset, so first-input attribution cannot pass.
    #[test]
    fn pass2_decode_failure_names_the_input_and_frame() {
        let dir = crate::path_guard::tests::scratch();
        let (a, b) = fixtures(dir.path());
        let plan = plan(&a, &b);
        let (entry, overlap) = entry_of(&plan, ElementKind::Node, 1);
        assert!(overlap, "B's node blob overlaps A's");
        assert!(entry.file_offset > 0, "the header blob precedes it");

        // Keep the length and the BlobHeader; make the Blob unparseable.
        let mut bytes = std::fs::read(&b).expect("read B");
        let start = usize::try_from(entry.file_offset).expect("offset");
        let header_len = u32::from_be_bytes(bytes[start..start + 4].try_into().expect("prefix"));
        let payload = start + 4 + header_len as usize;
        let end = start + usize::try_from(entry.frame_len).expect("frame len");
        bytes[payload..end].fill(0xff);
        std::fs::write(&b, &bytes).expect("corrupt B");

        let mut sources = open_sources(&plan.inputs).expect("open inputs");
        let err = match read_block(&mut sources[entry.file_index], entry, &mut Vec::new()) {
            Ok(_) => panic!("a corrupt blob must fail pass 2"),
            Err(err) => err,
        };
        assert!(
            matches!(err.kind(), crate::ErrorKind::WireFormat { .. }),
            "expected the Blob decode failure, got {err:?}"
        );
        assert_eq!(err.source_path(), Some(b.as_path()));
        assert_eq!(
            err.byte_offset(),
            Some(crate::ByteOffset(entry.file_offset))
        );
    }

    /// A passthrough frame that is gone when pass 2 re-reads it (the input
    /// was truncated between passes) fails located at its input and offset.
    #[test]
    fn pass2_read_failure_names_the_input_and_frame() {
        let dir = crate::path_guard::tests::scratch();
        let (a, b) = fixtures(dir.path());
        let plan = plan(&a, &b);
        let (entry, overlap) = entry_of(&plan, ElementKind::Relation, 1);
        assert!(!overlap, "B's relation blob passes through");

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&b)
            .expect("open B");
        file.set_len(entry.file_offset)
            .expect("truncate B at the relation frame");
        drop(file);

        let mut sources = open_sources(&plan.inputs).expect("open inputs");
        let err = match read_frame_into(&mut sources[entry.file_index], entry, &mut Vec::new()) {
            Ok(()) => panic!("a missing frame must fail pass 2"),
            Err(err) => err,
        };
        assert!(
            matches!(err.kind(), crate::ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
            "expected the frame read to hit end of file, got {err:?}"
        );
        assert_eq!(err.source_path(), Some(b.as_path()));
        assert_eq!(
            err.byte_offset(),
            Some(crate::ByteOffset(entry.file_offset))
        );
    }

    #[test]
    fn a_new_run_starts_after_the_previous_one_ends() {
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
