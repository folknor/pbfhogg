//! Sort a PBF file into standard order. Equivalent to `osmium sort`.
//!
//! Blob-level permutation sort through the merge engine shared with
//! `cat --dedupe` (`commands::blob_merge`): index each blob's element type +
//! ID range, order the index, write in order. Non-overlapping blobs pass
//! through as raw bytes; overlapping blobs are decoded and merged via a
//! streaming sweep. Memory is O(num_blobs) for non-overlapping inputs, plus
//! each active sweep's overlap depth and a bounded window of re-encoded
//! blocks when blobs overlap.

use std::path::Path;

use crate::commands::blob_merge::{self, IndexPolicy};
use crate::read::input::PinnedInput;
use crate::write::WriteOptions;

use super::{HeaderOverrides, Result, writer_from_header_bytes};

/// Statistics from a sort operation.
pub struct SortStats {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    /// Elements in passthrough blobs whose count came from unverified
    /// `BlobHeader.indexdata` rather than from a decode: a blob of a
    /// declared-sorted indexed input is never decoded, so its count is the
    /// file's own claim about itself, reported apart from the observed
    /// counts so a lying indexdata cannot move them unseen.
    pub passthrough_claimed: u64,
    pub blobs_passthrough: u64,
    pub blobs_rewritten: u64,
    /// Blobs holding no element, left out of the output.
    pub blobs_omitted: u64,
    /// Every input data blob: passthrough, rewritten and omitted together.
    pub blobs_total: u64,
}

impl SortStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Sorted {} nodes, {} ways, {} relations ({} blobs: {} passthrough, {} rewritten{})",
            self.nodes,
            self.ways,
            self.relations,
            self.blobs_total,
            self.blobs_passthrough,
            self.blobs_rewritten,
            omitted_clause(self.blobs_omitted),
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

/// The summary's omitted count, shown only when there is one.
pub(crate) fn omitted_clause(omitted: u64) -> String {
    if omitted == 0 {
        String::new()
    } else {
        format!(", {omitted} omitted")
    }
}

/// Sort a PBF file into standard order (nodes -> ways -> relations, by ID).
///
/// Uses blob-level permutation sort: O(num_blobs) memory. Non-overlapping
/// blobs are passed through as raw bytes; overlapping blobs are decoded,
/// sorted, and re-encoded.
///
/// The input is read in several passes, so it must be a regular file
/// (`InvalidArgument` otherwise), and is pinned for the run
/// (see [`PinnedInput`](crate::read::input::PinnedInput)): where a later
/// open cannot reach the pinned file, an input replaced mid-run fails with
/// `InputReplaced`.
pub fn sort(
    input: &Path,
    output: crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<SortStats> {
    host.cancel.check()?;
    let result = sort_to(input, &output, opts, overrides, host)?;
    output.publish(&host.cancel)?;
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
    // Every pass below opens the input again, each through its own open
    // file description (the front read, pass 1's header walker and pass 2's
    // source), and each is derived from this one pin, so a file renamed over
    // the input mid-run cannot be read partly from each: where
    // `/proc/self/fd` reopens the pin every open reaches the pinned file,
    // otherwise on Unix a replaced input fails the run (`PinnedInput` says
    // what it does off Unix). Taken before any other open of it.
    let pin = PinnedInput::pin(input, host)?;
    // The indexdata precondition is answered by a front read of its own,
    // dropped at once: pass 1's walker reads the header again on its own
    // `FADV_RANDOM` description.
    let indexed = crate::commands::input_front::InputFront::open(&pin, direct_io, host)?
        .require_indexdata(
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

    if let Ok(meta) = pin.metadata() {
        crate::debug::emit_counter("sort_total_bytes_in", meta.len());
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
    // Any input order is accepted (sorting it is the point). The walk
    // visits every header anyway, so it refuses a partially indexed input
    // at no extra read; a wholly unindexed one takes the payload scan.
    let (headers, indexed) = blob_merge::index_inputs(
        std::slice::from_ref(&pin),
        &IndexPolicy {
            require_sorted: None,
            refuse_partial_index: true,
        },
        host,
    )?;
    crate::debug::emit_marker("SORT_INDEX_BUILD_END");
    let blobs_total = indexed.frames.len() as u64 + indexed.omitted;
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "sort.indexed",
        "  {blobs_total} OSMData blobs indexed"
    );
    crate::debug::emit_counter("sort_blobs_total", blobs_total);

    crate::debug::emit_marker("SORT_OVERLAP_DETECT_START");
    let plan = blob_merge::plan(indexed);
    crate::debug::emit_marker("SORT_OVERLAP_DETECT_END");
    // `sort_blobs_overlap` counts blobs whose range overlaps a same-kind
    // neighbour; blobs rewritten only for intra-blob disorder are counted
    // apart, so the two attributions stay disjoint.
    let overlap_count = plan.overlapping;
    let intra_unsorted_blobs = plan.intra_unsorted;
    crate::debug::emit_counter("sort_blobs_overlap", overlap_count);
    crate::debug::emit_counter("sort_blobs_intra_unsorted", intra_unsorted_blobs);
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
    crate::debug::emit_counter("sort_blobs_restructured", plan.restructured);
    crate::debug::emit_counter("sort_blobs_omitted", plan.omitted);
    crate::debug::emit_counter("sort_false_indexdata_replaced", plan.false_claims_replaced);
    if plan.restructured > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "sort.restructured",
            "{} blobs held blocks no single index describes (kinds changing across \
             groups, or a group carrying more than one element field); each kind's \
             elements were decoded and re-encoded in type order, as overlapping blobs are",
            plan.restructured
        );
    }
    if plan.omitted > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "sort.omitted",
            "{} blobs held no elements and were omitted, with any unknown fields they \
             carried",
            plan.omitted
        );
    }
    if plan.false_claims_replaced > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "sort.false_indexdata",
            "{} blobs carried indexdata their payload contradicts; they were copied \
             under indexdata and tagdata scanned from the payload",
            plan.false_claims_replaced
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
    let header_bytes = super::output_header::build(
        &headers,
        &super::output_header::OutputFacts::sorted(plan.ways()),
        overrides,
        host,
    )?
    .bytes;
    let mut writer = writer_from_header_bytes(output, compression, &header_bytes, backend, host)?;
    crate::debug::emit_marker("SORT_WRITER_SETUP_END");

    let sources = blob_merge::open_sources(vec![pin])?;
    crate::debug::emit_counter("sort_overlap_runs", plan.runs.len());

    // Overlap runs are swept on worker threads while this loop writes, so
    // SORT_WRITE_LOOP spans both (the separate SORT_OVERLAP_PARALLEL phase of
    // the earlier collect-then-write shape no longer exists).
    crate::debug::emit_marker("SORT_WRITE_LOOP_START");
    let counts = blob_merge::write_plan(
        &plan,
        &sources,
        &mut writer,
        false,
        backend.supports_copy_range(),
        host,
    )?;
    crate::debug::emit_marker("SORT_WRITE_LOOP_END");

    crate::debug::emit_counter("sort_blobs_passthrough", counts.blobs_passthrough);
    crate::debug::emit_counter("sort_blobs_rewritten", counts.blobs_rewritten);
    crate::debug::emit_counter("sort_copy_range_calls", counts.copy_range_calls);
    crate::debug::emit_counter("sort_copy_range_coalesced", counts.copy_range_coalesced);

    crate::debug::emit_marker("SORT_FLUSH_START");
    // WAIT_WRITER spans the main thread's wait on the writer worker
    // to drain the coalesced `copy_file_range` (or its uring / userspace
    // fallback equivalent). On already-sorted input this is ~94 %
    // of wall, which `brokkr sidecar --durations` shows as the WAIT_WRITER
    // span (`--stalls` reads `*_wait_ns` counters, not marker pairs).
    crate::debug::emit_marker("WAIT_WRITER_START");
    writer.flush()?;
    crate::debug::emit_marker("WAIT_WRITER_END");
    crate::debug::emit_marker("SORT_FLUSH_END");
    if let Ok(meta) = std::fs::metadata(output.path()) {
        crate::debug::emit_counter("sort_total_bytes_out", meta.len());
    }
    crate::debug::emit_marker("SORT_PASS2_END");
    Ok(SortStats {
        nodes: counts.nodes,
        ways: counts.ways,
        relations: counts.relations,
        passthrough_claimed: counts.passthrough_claimed,
        blobs_passthrough: counts.blobs_passthrough,
        blobs_rewritten: counts.blobs_rewritten,
        blobs_omitted: plan.omitted,
        blobs_total: plan.blobs_total(),
    })
}
