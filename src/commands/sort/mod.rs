//! Sort a PBF file into standard order. Equivalent to `osmium sort`.
//!
//! Blob-level permutation sort through the merge engine shared with
//! `cat --dedupe` (`commands::blob_merge`): index each blob's element type +
//! ID range, order the index, write in order. Non-overlapping blobs pass
//! through as raw bytes; overlapping blobs are decoded and merged via a
//! streaming sweep. Memory is O(num_blobs) for non-overlapping inputs, plus
//! each active sweep's overlap depth and a bounded window of re-encoded
//! blocks when blobs overlap.

use crate::commands::blob_merge::{self, IndexPolicy};
use crate::read::input::InputSource;
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
/// (see [`InputSource::pin`](crate::read::input::InputSource::pin)): a path
/// is pinned here, a caller's [`PinnedInput`](crate::read::input::PinnedInput) is used as it is. Where a later
/// open cannot reach the pinned file, an input replaced mid-run fails with
/// `InputReplaced`.
pub fn sort<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<SortStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    sort_with(
        input,
        output,
        &SortOptions {
            write: *opts,
            distrust_metadata: false,
        },
        overrides,
        host,
    )
}

/// Configuration for [`sort_with`].
#[derive(Clone, Copy, Debug)]
pub struct SortOptions {
    /// Output compression, backend and the shared input flags.
    pub write: WriteOptions,
    /// Believe no claim the input makes about itself: decode every blob,
    /// checking its order and scanning its kind, ID range, count, bbox and
    /// tag keys, as for an input whose header does not claim
    /// `Sort.Type_then_ID`. A blob whose indexdata or tagdata the scan
    /// contradicts is reframed under the scanned ones (every other header
    /// field kept); an internally unsorted blob is rewritten in order. The
    /// repair for a file refused as `Unsorted` while its header claims
    /// sorted, which the default fast path copies unchanged because it
    /// believes that claim and the blobs' indexdata. Waives the indexdata
    /// precondition (`force` is not needed alongside it) and accepts a
    /// partially indexed input.
    pub distrust_metadata: bool,
}

/// [`sort`] with every option: see [`SortOptions`].
pub fn sort_with<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    opts: &SortOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<SortStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = sort_to(input, &output, opts, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`sort_with`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub(crate) fn sort_to<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::BoundOutput,
    opts: &SortOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<SortStats> {
    let SortOptions {
        write:
            WriteOptions {
                compression,
                backend,
                direct_reads: direct_io,
                force,
            },
        distrust_metadata,
    } = *opts;
    // Every pass below opens the input again, each through its own open
    // file description (the front read, pass 1's header walker and pass 2's
    // source), and each is derived from this one pin, so a file renamed over
    // the input mid-run cannot be read partly from each: where
    // `/proc/self/fd` reopens the pin every open reaches the pinned file,
    // otherwise on Unix a replaced input fails the run (`PinnedInput` says
    // what it does off Unix). Taken before any other open of it.
    let pin = input.into().pin(host)?;
    // The indexdata precondition is answered by a front read of its own,
    // dropped at once: pass 1's walker reads the header again on its own
    // description (`FADV_RANDOM`, or `O_DIRECT` under `--direct-io`).
    // Distrusting metadata reads no indexdata, so it needs none: pass 1
    // decodes every blob whatever it carries.
    let indexed = distrust_metadata
        || crate::commands::input_front::InputFront::open(&pin, direct_io, host)?
            .require_indexdata(
                force,
                "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
                 decompressed to scan element IDs (significantly slower).",
                host,
            )?;
    if distrust_metadata {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "sort.distrust_metadata",
            "believing no header sort claim, indexdata or tagdata; pass 1 decodes every \
             blob payload"
        );
    }
    if !indexed {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "sort.unindexed_fallback",
            "sorting without indexdata; every blob must be decompressed to scan element IDs"
        );
        host.invocation.counter("sort_unindexed_fallback", 1);
    }

    if let Ok(meta) = pin.metadata() {
        host.invocation.counter("sort_total_bytes_in", meta.len());
    }

    // Pass 1: Build blob index
    host.invocation.marker("SORT_PASS1_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "sort.pass1",
        "Pass 1: indexing blobs..."
    );
    host.invocation.marker("SORT_INDEX_BUILD_START");
    // Any input order is accepted (sorting it is the point). The walk
    // refuses a partially indexed input, as every command-input walk does;
    // a wholly unindexed one takes the payload scan.
    let (headers, indexed) = blob_merge::index_inputs(
        std::slice::from_ref(&pin),
        &IndexPolicy {
            require_sorted: None,
            accept_partial_index: false,
            distrust_metadata,
        },
        direct_io,
        host,
    )?;
    host.invocation.marker("SORT_INDEX_BUILD_END");
    let blobs_total = indexed.frames.len() as u64 + indexed.omitted;
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "sort.indexed",
        "  {blobs_total} OSMData blobs indexed"
    );
    host.invocation.counter("sort_blobs_total", blobs_total);

    host.invocation.marker("SORT_OVERLAP_DETECT_START");
    let plan = blob_merge::plan(indexed);
    host.invocation.marker("SORT_OVERLAP_DETECT_END");
    // `sort_blobs_overlap` counts blobs whose range overlaps a same-kind
    // neighbour; blobs rewritten only for intra-blob disorder are counted
    // apart, so the two attributions stay disjoint.
    let overlap_count = plan.overlapping;
    let intra_unsorted_blobs = plan.intra_unsorted;
    host.invocation.counter("sort_blobs_overlap", overlap_count);
    host.invocation
        .counter("sort_blobs_intra_unsorted", intra_unsorted_blobs);
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
    host.invocation
        .counter("sort_blobs_restructured", plan.restructured);
    host.invocation.counter("sort_blobs_omitted", plan.omitted);
    host.invocation
        .counter("sort_false_indexdata_replaced", plan.false_claims_replaced);
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
            "{} blobs carried indexdata their payload contradicts (or, when distrusting \
             metadata, tagdata); they were copied under indexdata and tagdata scanned \
             from the payload",
            plan.false_claims_replaced
        );
    }

    host.invocation.marker("SORT_PASS1_END");

    // Pass 2: Write in sorted order
    host.invocation.marker("SORT_PASS2_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "sort.pass2",
        "Pass 2: writing sorted output..."
    );
    host.invocation.marker("SORT_WRITER_SETUP_START");
    let header_bytes = super::output_header::build(
        &headers,
        &super::output_header::OutputFacts::sorted(plan.ways()),
        overrides,
        host,
    )?
    .bytes;
    let mut writer = writer_from_header_bytes(output, compression, &header_bytes, backend, host)?;
    host.invocation.marker("SORT_WRITER_SETUP_END");

    let sources =
        blob_merge::open_sources(vec![pin], &plan, direct_io, backend.supports_copy_range())?;
    host.invocation
        .counter("sort_overlap_runs", plan.runs.len());

    // Overlap runs are swept on worker threads while this loop writes, so
    // SORT_WRITE_LOOP spans both (the separate SORT_OVERLAP_PARALLEL phase of
    // the earlier collect-then-write shape no longer exists).
    host.invocation.marker("SORT_WRITE_LOOP_START");
    let counts = blob_merge::write_plan(&plan, &sources, &mut writer, false, compression, host)?;
    host.invocation.marker("SORT_WRITE_LOOP_END");

    host.invocation
        .counter("sort_blobs_passthrough", counts.blobs_passthrough);
    host.invocation
        .counter("sort_blobs_rewritten", counts.blobs_rewritten);
    host.invocation
        .counter("sort_copy_range_calls", counts.copy_range_calls);
    host.invocation
        .counter("sort_copy_range_coalesced", counts.copy_range_coalesced);

    host.invocation.marker("SORT_FLUSH_START");
    // WAIT_WRITER spans the main thread's wait on the writer worker
    // to drain the coalesced `copy_file_range` (or its uring / userspace
    // fallback equivalent). On already-sorted input this is ~94 %
    // of wall, which `brokkr sidecar --durations` shows as the WAIT_WRITER
    // span (`--stalls` reads `*_wait_ns` counters, not marker pairs).
    host.invocation.marker("WAIT_WRITER_START");
    writer.flush()?;
    host.invocation.marker("WAIT_WRITER_END");
    host.invocation.marker("SORT_FLUSH_END");
    if let Ok(meta) = std::fs::metadata(output.path()) {
        host.invocation.counter("sort_total_bytes_out", meta.len());
    }
    host.invocation.marker("SORT_PASS2_END");
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

#[cfg(test)]
#[cfg(feature = "linux-direct-io")]
mod tests {
    use super::*;
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::read::header_walker::tests::direct_opens;
    use crate::test_support::direct_reads_supported;
    use crate::writer::{Compression, PbfWriter};
    use std::path::Path;

    /// An indexed input whose header does not claim sorted order, so pass 1
    /// reads and scans every payload, with two overlapping node blobs and a
    /// way, so pass 2 sweeps a run.
    fn write_unsorted(path: &Path) {
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().build().expect("header"))
            .expect("write header");
        let none = std::iter::empty::<(&str, &str)>;
        let mut bb = BlockBuilder::new();
        for ids in [&[5_i64, 9][..], &[1, 2, 7][..]] {
            for &id in ids {
                bb.add_node(id, 10, 20, [("k", "v")].iter().copied(), None);
            }
            let bytes = bb.take().expect("encode").expect("block");
            writer.write_primitive_block(bytes).expect("write block");
        }
        bb.add_way(10, none(), &[1, 2], None);
        let bytes = bb.take().expect("encode").expect("block");
        writer.write_primitive_block(bytes).expect("write block");
        writer.flush().expect("flush fixture");
    }

    fn run(input: &Path, output: &Path, direct_reads: bool) -> Vec<u8> {
        let host = crate::host::Host::entered_for_tests();
        let out = crate::output::Output::new(
            output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .expect("output");
        let opts = WriteOptions {
            direct_reads,
            ..WriteOptions::default()
        };
        let stats = sort(input, out, &opts, &HeaderOverrides::default(), &host).expect("sort");
        assert_eq!((stats.nodes, stats.ways, stats.blobs_rewritten), (5, 1, 2));
        std::fs::read(output).expect("read output")
    }

    /// `--direct-io` reaches pass 1: its walk opens the input `O_DIRECT` (the
    /// count is of direct walks, which the front read never opens), and the
    /// output is the buffered run's, byte for byte.
    #[test]
    fn direct_reads_walk_pass_1_direct() {
        let dir = crate::path_guard::tests::scratch();
        if !direct_reads_supported(dir.path(), "the whole test") {
            return;
        }
        let input = dir.path().join("unsorted.osm.pbf");
        write_unsorted(&input);
        let before = direct_opens();
        let buffered = run(&input, &dir.path().join("buffered.osm.pbf"), false);
        assert_eq!(direct_opens(), before, "a buffered run walked direct");
        let direct = run(&input, &dir.path().join("direct.osm.pbf"), true);
        assert_eq!(direct_opens(), before + 1, "pass 1 did not walk direct");
        assert_eq!(direct, buffered);
    }
}
