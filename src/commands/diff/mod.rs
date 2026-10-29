//! Compare two PBF files and output human-readable differences. Equivalent to `osmium diff`.
//!
//! Requires both inputs to declare `Sort.Type_then_ID`. Indexed inputs are
//! compared by the block-pair merge over ID-range shards (`shard`,
//! `sharded`, one shard at `-j 1`); inputs without indexdata by the
//! element-level merge over [`StreamingBlocks`] cursors. Memory is bounded
//! by the decoded blocks in flight either way.
//!
//! # Design: content equality, not version ordering
//!
//! Unlike osmium's diff, which uses a version/timestamp comparator to order elements and
//! can produce wrong output when inputs have mismatched metadata (osmium-tool#93), pbfhogg
//! diff uses content equality: two elements with the same type+ID are compared field by
//! field (coordinates, tags, refs, members). This makes diff output deterministic regardless
//! of whether metadata (version, timestamp, changeset, uid) is present, partial, or absent.

pub mod derive;
mod seq;
mod shard;
mod sharded;

use super::Result;
use super::input_front::sorted_and_indexed;
use crate::Element;
use crate::ElementKind;
use crate::coord_fmt::{format_coord, from_decimicro};
use crate::osc::merge_join::{BlockMergeAction, MergeSink, StreamingBlocks, stream_merge_phase};
use crate::owned::TypeFilter;
use crate::read::input::{InputSource, PinnedInput};
use crate::text_quote::{MemberLabel, Quoted};
use std::collections::HashMap;
use std::io::Write;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// `require_sorted` is defined in `super::mod` and re-used here.

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Options for the diff command.
pub struct DiffOptions {
    /// Hide unchanged elements (show only created/modified/deleted).
    pub suppress_common: bool,
    /// Show detailed changes for modified elements.
    pub verbose: bool,
    /// Restrict the comparison to these element types. `None` = all.
    pub type_filter: Option<TypeFilter>,
    /// Number of ID-range shards (and worker threads) for the block-pair
    /// merge of indexed inputs. `Fixed(1)` runs one shard on the calling
    /// thread, writing the output directly; `Fixed(n)` dispatches up to `n`
    /// shards across worker threads; `Auto` uses one shard per core the host
    /// reports (one when it reports none). The answer is the same at any
    /// count.
    pub parallelism: crate::host::Parallelism,
}

impl DiffOptions {
    /// The shard count this run uses: the fixed count, else one per core.
    fn shards(&self, host: &crate::host::Entered<'_>) -> usize {
        resolve_shards(self.parallelism, host)
    }
}

/// The shard count of a diff or derive (`--format osc`) run: the fixed
/// count, else one per core the host reports (one when it reports none).
pub(crate) fn resolve_shards(
    parallelism: crate::host::Parallelism,
    host: &crate::host::Entered<'_>,
) -> usize {
    parallelism.resolve(|| {
        host.resources
            .parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
    })
}

/// Statistics from a diff operation.
#[derive(Debug, Default)]
pub struct DiffStats {
    pub common: u64,
    pub created: u64,
    pub modified: u64,
    pub deleted: u64,
}

impl DiffStats {
    /// Returns true if any differences were found.
    pub fn has_differences(&self) -> bool {
        self.created > 0 || self.modified > 0 || self.deleted > 0
    }

    /// Fold another run's (a shard's) counts into these.
    pub(crate) fn add(&mut self, other: &Self) {
        self.common += other.common;
        self.created += other.created;
        self.modified += other.modified;
        self.deleted += other.deleted;
    }

    /// Print default summary to stderr (pbfhogg format).
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self.created + self.modified + self.deleted;
        if total == 0 {
            writeln!(w, "Files are identical ({} common elements)", self.common)?;
        } else {
            writeln!(
                w,
                "{total} differences: {} created, {} modified, {} deleted ({} common)",
                self.created, self.modified, self.deleted, self.common,
            )?;
        }

        Ok(())
    }

    /// Print osmium-compatible summary to stderr (left/right/same/different).
    pub fn write_osmium_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let left = self.common + self.modified + self.deleted;
        let right = self.common + self.modified + self.created;
        let different = self.created + self.modified + self.deleted;
        writeln!(
            w,
            "Summary: left={left} right={right} same={} different={different}",
            self.common,
        )?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compare two sorted PBF files and write human-readable differences.
///
/// Holds a bounded number of decoded blocks at a time, whatever the input
/// size. Requires both inputs to declare
/// `Sort.Type_then_ID` - returns an actionable error if either is unsorted.
///
/// `scratch` is the run's scratch directory, used by a multi-shard run for
/// its shard files and removed when the call returns. A caller whose options
/// run one shard (`Fixed(1)`) writes no scratch and passes `None`; `None`
/// with more than one shard is refused, before any input is read. A caller that wants only the counts calls
/// [`diff_stats`], which shards without scratch.
///
/// Every route opens each input more than once, so both must be regular
/// files (`InvalidArgument` otherwise) and are pinned for
/// the run once the options are accepted, `old` first, before either is
/// read (see [`PinnedInput`] and [`InputSource::pin`]). A path spelled
/// exactly as the other shares its pin, so `diff a a` compares one file
/// with itself; a caller may pin either input first and pass the pin, which
/// is used as it is. Where a later open cannot reach the pinned file, an
/// input replaced mid-run fails with `InputReplaced`; text already written
/// to `output` by then stays written.
#[hotpath::measure]
pub fn diff<'a, 'b>(
    old: impl Into<InputSource<'a>>,
    new: impl Into<InputSource<'b>>,
    output: &mut impl Write,
    options: &DiffOptions,
    direct_io: bool,
    host: &crate::host::Host,
    scratch: Option<crate::scratch::RunScratch>,
) -> Result<DiffStats> {
    let host = &host.enter();
    host.invocation.check()?;
    let jobs = options.shards(host);
    if jobs > 1 && scratch.is_none() {
        return Err(crate::Error::invalid_argument(
            "diff: a run that may use more than one shard needs a scratch directory; \
             without one it must run a single shard",
        ));
    }
    let [old, new] = pin_pair(old.into(), new.into(), host)?;
    let result = compare(
        &old,
        &new,
        Report::Text {
            out: output,
            scratch: scratch.as_ref(),
        },
        options,
        jobs,
        direct_io,
        host,
    );
    // The scratch directory is removed before the completion decision, so a
    // cancellation observed during that removal is still reported as one.
    drop(scratch);
    let stats = result?;
    // The completion decision: a cancellation observed by now is reported
    // as one, whatever reached the caller's writer.
    host.invocation.check()?;
    Ok(stats)
}

/// Count the differences between two sorted PBF files without producing
/// any text.
///
/// The comparison is the one [`diff`] runs with `suppress_common` on,
/// `verbose` off and the same `type_filter` and `parallelism`: the same
/// paths (parallel shards for indexed inputs, the sequential fallbacks
/// otherwise) and the same input checks. Like `diff -c`, it skips a fresh
/// pair of byte-equal blobs without decoding it, so `common` counts such a
/// pair by the blobs' claimed `index.count` and a corrupt blob that is
/// byte-identical on both sides is not noticed; `created`, `modified` and
/// `deleted` are exact either way. Only the formatting is skipped, so no
/// output or scratch I/O happens at all, and a sharded comparison needs no
/// scratch directory.
///
/// The inputs are pinned as in [`diff`], with the same refusals.
#[hotpath::measure]
pub fn diff_stats<'a, 'b>(
    old: impl Into<InputSource<'a>>,
    new: impl Into<InputSource<'b>>,
    type_filter: Option<TypeFilter>,
    parallelism: crate::host::Parallelism,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<DiffStats> {
    let host = &host.enter();
    host.invocation.check()?;
    // Nothing is printed, so byte-equal blobs may be skipped (see above).
    let options = DiffOptions {
        suppress_common: true,
        verbose: false,
        type_filter,
        parallelism,
    };
    let jobs = options.shards(host);
    let [old, new] = pin_pair(old.into(), new.into(), host)?;
    let stats = compare(
        &old,
        &new,
        Report::<std::io::Sink>::Count,
        &options,
        jobs,
        direct_io,
        host,
    )?;
    host.invocation.check()?;
    Ok(stats)
}

/// Pin both inputs, `old` first, before either is read: a path spelled
/// exactly as the other shares its pin, a caller's pin is used as it is
/// (see `read::input::pin_sources`). Shared by the text diff and derive.
pub(super) fn pin_pair(
    old: InputSource<'_>,
    new: InputSource<'_>,
    host: &crate::host::Entered<'_>,
) -> Result<[PinnedInput; 2]> {
    let pins = crate::read::input::pin_sources(&[old, new], host)?;
    <[PinnedInput; 2]>::try_from(pins)
        .map_err(|_| crate::Error::internal("diff: two inputs must yield two pins"))
}

/// What a comparison produces besides its counts.
enum Report<'a, W> {
    /// Diff text written to `out`. `scratch` holds a multi-shard run's shard
    /// files; `None` only when the run is single-shard, which [`diff`]
    /// enforces.
    Text {
        out: &'a mut W,
        scratch: Option<&'a crate::scratch::RunScratch>,
    },
    /// Counts only: nothing is formatted and no scratch is written.
    Count,
}

/// The comparison shared by [`diff`] and [`diff_stats`]: input checks, path
/// selection and the per-path drivers. The caller owns the entry and
/// completion cancellation checks, and has pinned both inputs: every open
/// below is derived from a pin.
fn compare<W: Write>(
    old: &PinnedInput,
    new: &PinnedInput,
    report: Report<'_, W>,
    options: &DiffOptions,
    jobs: usize,
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<DiffStats> {
    let filter = TypeFilter::or_all(options.type_filter);
    let (old_path, new_path) = (old.path(), new.path());

    // Sorted headers + indexdata from one front read of each input.
    let (old_sorted, old_indexed) = sorted_and_indexed(old, direct_io, host)?;
    let (new_sorted, new_indexed) = sorted_and_indexed(new, direct_io, host)?;
    super::require_sorted(
        old_sorted,
        old_path,
        "Old PBF",
        "Streaming diff merge-walks both inputs in id order.",
    )?;
    super::require_sorted(
        new_sorted,
        new_path,
        "New PBF",
        "Streaming diff merge-walks both inputs in id order.",
    )?;
    let both_indexed = old_indexed && new_indexed;
    if jobs > 1 && !both_indexed {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "diff.sequential_fallback",
            "Using sequential comparison: missing indexdata"
        );
        host.invocation.counter("diff_sequential_fallback", 1);
    }

    host.invocation.marker("DIFF_SCAN_START");

    // Path selection depends on the inputs and the shard count only, never on
    // whether text is wanted: a count-only run shards exactly as a text run.
    let (output, scratch) = match report {
        Report::Text { out, scratch } => (Some(out), scratch),
        Report::Count => (None, None),
    };
    let stats = if both_indexed {
        diff_block_pair(
            old, new, output, scratch, options, &filter, jobs, direct_io, host,
        )?
    } else {
        diff_element_stream(old, new, output, options, direct_io, &filter, host)?
    };
    Ok(finish_scan(stats, &host.invocation))
}

/// Close the scan: the end marker and the result counters, on every path.
fn finish_scan(stats: DiffStats, invocation: &crate::host::Recorder) -> DiffStats {
    invocation.marker("DIFF_SCAN_END");
    invocation.counter("diff_common", stats.common);
    invocation.counter("diff_created", stats.created);
    invocation.counter("diff_modified", stats.modified);
    invocation.counter("diff_deleted", stats.deleted);
    stats
}

/// The diff of two indexed inputs: the header walk, then the block-pair
/// merge over up to `jobs` ID-range shards per kind (see `shard` and
/// `sharded`). Zero String allocation for unchanged elements.
///
/// `output` is `None` for a count-only run: every selected block is still
/// read, decoded and merged exactly as for text (the byte-equal fast path
/// follows `suppress_common`, not the absence of text, and `diff_stats`
/// sets it), only nothing is
/// formatted and no scratch is written. A text run of more than one shard
/// writes each shard's text to `scratch` and streams it to `output` in
/// shard order; a phase of one shard writes `output` directly.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn diff_block_pair<W: Write>(
    old: &PinnedInput,
    new: &PinnedInput,
    output: Option<&mut W>,
    scratch: Option<&crate::scratch::RunScratch>,
    options: &DiffOptions,
    filter: &TypeFilter,
    jobs: usize,
    // Not honoured yet: the walk and the shard reads pread through the
    // walker's buffered descriptor at every shard count. Kept as the place
    // the flag reaches once those reads honour it.
    _direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<DiffStats> {
    // DIFF_SCAN_START/END and the diff_* result counters are the caller's.
    let walked = shard::walk_pair(old, new, host)?;
    if walked.has_negative && jobs > 1 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "diff.negative_id_fallback",
            "Using a single shard: negative IDs cannot use ID-range shards"
        );
        host.invocation.counter("diff_negative_id_fallback", 1);
    }

    // Scratch shard files live in the run's scratch directory, which the
    // caller placed (the CLI's --tmp-dir, else beside the output).
    // No input-size-based free-space refusal: compressed PBF sizes and index
    // counts do not bound changed text bytes (identical inputs emit none).
    // Exact sizing would repeat the measured comparison/serialization pass.
    // Shards coexist until their kind is drained, then are removed one by
    // one; the caller's arbitrary Write destination may share this
    // filesystem or consume no disk at all, so twice the input size is not a
    // space budget. Keep write errors authoritative rather than rejecting on
    // that guess.
    let phases: Vec<sharded::Phase> = [
        (
            ElementKind::Node,
            filter.nodes,
            "DIFF_PHASE_NODE_START",
            "DIFF_PHASE_NODE_END",
        ),
        (
            ElementKind::Way,
            filter.ways,
            "DIFF_PHASE_WAY_START",
            "DIFF_PHASE_WAY_END",
        ),
        (
            ElementKind::Relation,
            filter.relations,
            "DIFF_PHASE_REL_START",
            "DIFF_PHASE_REL_END",
        ),
    ]
    .into_iter()
    .filter(|&(_, enabled, _, _)| enabled)
    .map(|(kind, _, start_marker, end_marker)| sharded::Phase {
        kind,
        start_marker,
        end_marker,
    })
    .collect();

    let mut report = TextReport {
        sink: DiffSink::new(output, options),
        scratch,
    };
    let merge_stats = sharded::run(
        &walked,
        &phases,
        jobs,
        options.suppress_common,
        &mut report,
        host,
    )?;
    emit_merge_stats_counters(&merge_stats, &host.invocation);
    Ok(report.sink.stats)
}

/// Where a diff's merge actions go: the counts, and the text when there is
/// an output. One sink for every path - each shard of the block-pair merge
/// and the unindexed element stream - so a difference prints the same line
/// (and, under `verbose`, the same details) at any shard count and whether
/// or not the inputs carried indexdata.
struct DiffSink<'a, O: Write> {
    /// `None` for a count-only run.
    output: Option<O>,
    options: &'a DiffOptions,
    stats: DiffStats,
}

impl<'a, O: Write> DiffSink<'a, O> {
    fn new(output: Option<O>, options: &'a DiffOptions) -> Self {
        Self {
            output,
            options,
            stats: DiffStats::default(),
        }
    }
}

impl<O: Write> MergeSink for DiffSink<'_, O> {
    fn record(&mut self, kind: ElementKind, action: &BlockMergeAction<'_>) -> Result<()> {
        use crate::osc::merge_join::{element_id, element_version};
        let type_char = kind.type_char();
        match action {
            BlockMergeAction::BlobEqual(count) => {
                self.stats.common += count;
            }
            // A single-sided run's count is the merge's (claimed or, for a
            // clipped block, counted); count-only runs skip the element walk
            // that exists only to format each line.
            BlockMergeAction::BlobOldOnly(run) => {
                if let Some(out) = self.output.as_mut() {
                    for elem in run.elements() {
                        let ver = element_version(&elem);
                        write_compact_line(out, '-', type_char, element_id(&elem), ver)?;
                    }
                }
                self.stats.deleted += run.count;
            }
            BlockMergeAction::BlobNewOnly(run) => {
                if let Some(out) = self.output.as_mut() {
                    for elem in run.elements() {
                        let ver = element_version(&elem);
                        write_compact_line(out, '+', type_char, element_id(&elem), ver)?;
                    }
                }
                self.stats.created += run.count;
            }
            BlockMergeAction::ElementEqual(e) => {
                if !self.options.suppress_common
                    && let Some(out) = self.output.as_mut()
                {
                    write_compact_line(out, ' ', type_char, element_id(e), element_version(e))?;
                }
                self.stats.common += 1;
            }
            BlockMergeAction::ElementModified { old, new } => {
                if let Some(out) = self.output.as_mut() {
                    let old_ver = element_version(old);
                    let new_ver = element_version(new);
                    write_modified_line(out, type_char, element_id(old), old_ver, new_ver)?;
                    if self.options.verbose {
                        write_modified_details_borrowed(out, old, new)?;
                    }
                }
                self.stats.modified += 1;
            }
            BlockMergeAction::ElementOldOnly(o) => {
                if let Some(out) = self.output.as_mut() {
                    write_compact_line(out, '-', type_char, element_id(o), element_version(o))?;
                }
                self.stats.deleted += 1;
            }
            BlockMergeAction::ElementNewOnly(n) => {
                if let Some(out) = self.output.as_mut() {
                    write_compact_line(out, '+', type_char, element_id(n), element_version(n))?;
                }
                self.stats.created += 1;
            }
        }
        Ok(())
    }
}

/// The text diff's side of a sharded comparison: the caller's writer (or
/// nothing, counting only) as the final sink, and one scratch text file per
/// shard of a multi-shard phase.
struct TextReport<'a, W: Write> {
    sink: DiffSink<'a, &'a mut W>,
    /// `None` for a count-only run, and for a text run that may use only
    /// one shard (which `diff` enforces).
    scratch: Option<&'a crate::scratch::RunScratch>,
}

/// One shard of a multi-shard text phase: its sink and its scratch file
/// (`None` for a count-only run, which formats nothing).
struct TextShard<'a> {
    sink: DiffSink<'a, std::io::BufWriter<std::fs::File>>,
    path: Option<crate::scratch::ScratchPath>,
}

impl MergeSink for TextShard<'_> {
    fn record(&mut self, kind: ElementKind, action: &BlockMergeAction<'_>) -> Result<()> {
        self.sink.record(kind, action)
    }
}

impl<'a, W: Write> sharded::ShardedReport for TextReport<'a, W> {
    type Shard = TextShard<'a>;
    type Done = (Option<crate::scratch::ScratchPath>, DiffStats);

    const WORKER: &'static str = "shard worker";

    fn inline(&mut self) -> &mut impl MergeSink {
        &mut self.sink
    }

    fn open_shard(&mut self, kind: ElementKind, idx: usize) -> Result<Self::Shard> {
        let options = self.sink.options;
        if self.sink.output.is_none() {
            return Ok(TextShard {
                sink: DiffSink::new(None, options),
                path: None,
            });
        }
        let scratch = self.scratch.ok_or_else(|| {
            crate::Error::invalid_argument(
                "diff: a run that may use more than one shard needs a scratch directory",
            )
        })?;
        let path = scratch.file(&format!("diff-par-{}-{idx}.txt.tmp", kind.type_char()))?;
        let file = crate::scratch::create_file(&path)?;
        Ok(TextShard {
            sink: DiffSink::new(Some(std::io::BufWriter::new(file)), options),
            path: Some(path),
        })
    }

    fn finish_shard(shard: Self::Shard) -> Result<Self::Done> {
        // BufWriter::into_inner propagates any deferred flush error; dropping
        // the unwrapped File closes it and the main thread reopens the path.
        if let Some(out) = shard.sink.output {
            drop(
                out.into_inner()
                    .map_err(|e| crate::Error::from(e.into_error()))?,
            );
        }
        Ok((shard.path, shard.sink.stats))
    }

    fn absorb(&mut self, done: Vec<Self::Done>, cancel: &crate::host::Recorder) -> Result<()> {
        match (self.sink.output.as_mut(), self.scratch) {
            (Some(out), Some(scratch)) => {
                emit_shard_outputs(&mut **out, done, &mut self.sink.stats, scratch, cancel)
            }
            _ => {
                for (_, stats) in done {
                    self.sink.stats.add(&stats);
                }
                Ok(())
            }
        }
    }

    #[cfg(feature = "test-hooks")]
    fn panic_hook(faults: &crate::test_hooks::FaultPlan) -> &std::sync::atomic::AtomicUsize {
        &faults.diff_shard_panic
    }

    #[cfg(feature = "test-hooks")]
    fn panic_message(idx: usize) -> String {
        format!("test-hooks: diff/parallel shard {idx} panicking")
    }
}

/// Stream every shard's temp file to `output` in shard order, folding each
/// shard's counts into `total`.
///
/// A failed write to `output` - the reader of a stdout pipe going away is
/// the ordinary case (`diff a b | head`) - stops the emission, but every
/// remaining shard's temp file is still removed before the error returns.
/// Returning on the first error left them all in the scratch directory
/// beside the old input, one per shard per run.
fn emit_shard_outputs(
    output: &mut impl Write,
    outputs: Vec<(Option<crate::scratch::ScratchPath>, DiffStats)>,
    total: &mut DiffStats,
    scratch: &crate::scratch::RunScratch,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    let mut first_err = None;
    for (text_path, stats) in outputs {
        let Some(text_path) = text_path else {
            return Err(crate::Error::internal(
                "diff: a shard of a text run wrote no text file",
            ));
        };
        if first_err.is_some() {
            scratch.remove(&text_path);
            continue;
        }
        if let Err(e) = append_and_cleanup(output, &text_path, scratch, cancel) {
            first_err = Some(e);
            continue;
        }
        total.add(&stats);
    }
    first_err.map_or(Ok(()), Err)
}

/// Copy a shard's scratch temp file to the caller's output writer and then
/// remove it - also when the copy fails, so a failed write does not leave
/// the temp file behind.
fn append_and_cleanup(
    out: &mut impl Write,
    src: &crate::scratch::ScratchPath,
    scratch: &crate::scratch::RunScratch,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    // The copy checks the token at every read of the shard file (a shard
    // can hold gigabytes of text, and stdout is not cancellation-aware).
    let copied = crate::scratch::open(src)
        .and_then(|f| std::io::copy(&mut crate::host::CancellableRead::new(f, cancel), out));
    scratch.remove(src);
    match copied {
        Ok(_) => Ok(()),
        // A cancellation is not a failure of this file: left as it is.
        Err(e) if crate::error::io_carries_cancellation(&e) => Err(e.into()),
        Err(e) => {
            Err(crate::Error::from(e)
                .context(format!("failed to copy scratch file {}", src.display())))
        }
    }
}

/// Emit `mergejoin_shadow_*` counters accumulated across all shards and
/// phases. Shadow-only - no code-path change. Shared by the text diff and
/// derive (`--format osc`), which both drive the block-pair merge and
/// report the same shadow view.
fn emit_merge_stats_counters(
    s: &crate::osc::merge_join::BlockPairMergeStats,
    invocation: &crate::host::Recorder,
) {
    invocation.counter("mergejoin_shadow_pairs_byte_equal", s.pairs_byte_equal);
    invocation.counter(
        "mergejoin_shadow_elements_byte_equal",
        s.elements_byte_equal,
    );
    invocation.counter(
        "mergejoin_shadow_pairs_overlapping_decoded",
        s.pairs_overlapping_decoded,
    );
    invocation.counter(
        "mergejoin_shadow_elements_overlapping_decoded",
        s.elements_overlapping_decoded,
    );
    invocation.counter("mergejoin_shadow_blobs_old_only", s.blobs_old_only);
    invocation.counter("mergejoin_shadow_elements_old_only", s.elements_old_only);
    invocation.counter("mergejoin_shadow_blobs_new_only", s.blobs_new_only);
    invocation.counter("mergejoin_shadow_elements_new_only", s.elements_new_only);
}

/// Fallback diff path for inputs without indexdata: the element-level
/// streaming merge, fed through the same [`DiffSink`] as the block-pair
/// path. `output` is `None` for a count-only run, which merges exactly the
/// same elements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn diff_element_stream<W: Write>(
    old: &PinnedInput,
    new: &PinnedInput,
    output: Option<&mut W>,
    options: &DiffOptions,
    direct_io: bool,
    filter: &TypeFilter,
    host: &crate::host::Entered<'_>,
) -> Result<DiffStats> {
    let pool = crate::osc::merge_join::merge_buffers(&host.invocation);
    let mut old_src =
        StreamingBlocks::new_sequential(old, direct_io, std::sync::Arc::clone(&pool), host)?;
    let mut new_src = StreamingBlocks::new_sequential(new, direct_io, pool, host)?;

    let mut sink = DiffSink::new(output, options);

    let phases = [
        (
            ElementKind::Node,
            filter.nodes,
            "DIFF_PHASE_NODE_START",
            "DIFF_PHASE_NODE_END",
        ),
        (
            ElementKind::Way,
            filter.ways,
            "DIFF_PHASE_WAY_START",
            "DIFF_PHASE_WAY_END",
        ),
        (
            ElementKind::Relation,
            filter.relations,
            "DIFF_PHASE_REL_START",
            "DIFF_PHASE_REL_END",
        ),
    ];
    for (kind, enabled, start, end) in phases {
        if enabled {
            host.invocation.marker(start);
            stream_merge_phase(&mut old_src, &mut new_src, kind, &mut sink)?;
            host.invocation.marker(end);
        } else {
            skip_kind(&mut old_src, &mut new_src, kind)?;
        }
    }

    Ok(sink.stats)
}

/// Advance both cursors past a kind the type filter excludes, wherever it
/// sits: before a selected kind, so the next phase starts at its own first
/// block, or after the last one, so a misplaced block of a selected kind in
/// the tail is refused rather than left unread. Excluded blocks are
/// classified and dropped without converting their elements; see
/// [`StreamingBlocks::skip_kind`] for what that does and does not check.
fn skip_kind(
    old_src: &mut StreamingBlocks,
    new_src: &mut StreamingBlocks,
    kind: ElementKind,
) -> Result<()> {
    old_src.skip_kind(kind)?;
    new_src.skip_kind(kind)
}

// ---------------------------------------------------------------------------
// Output formatting -- compact lines
// ---------------------------------------------------------------------------

pub(super) fn write_compact_line(
    output: &mut impl Write,
    prefix: char,
    type_char: char,
    id: i64,
    version: Option<i32>,
) -> Result<()> {
    match version {
        Some(v) => writeln!(output, "{prefix}{type_char}{id} v{v}")?,
        None => writeln!(output, "{prefix}{type_char}{id}")?,
    }
    Ok(())
}

pub(super) fn write_modified_line(
    output: &mut impl Write,
    type_char: char,
    id: i64,
    old_version: Option<i32>,
    new_version: Option<i32>,
) -> Result<()> {
    match (old_version, new_version) {
        (Some(ov), Some(nv)) if ov != nv => writeln!(output, "*{type_char}{id} v{ov} -> v{nv}")?,
        (_, Some(v)) => writeln!(output, "*{type_char}{id} v{v}")?,
        (Some(v), None) => writeln!(output, "*{type_char}{id} v{v}")?,
        (None, None) => writeln!(output, "*{type_char}{id}")?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Verbose details
// ---------------------------------------------------------------------------

/// Write verbose modification details using borrowed element references.
///
/// Every string is resolved through the checked lookups: a tag key, value
/// or member role whose string index is past the block's string table is an
/// error, not a silently shortened detail list.
pub(super) fn write_modified_details_borrowed(
    output: &mut dyn Write,
    old: &Element<'_>,
    new: &Element<'_>,
) -> Result<()> {
    match (old, new) {
        (Element::DenseNode(_) | Element::Node(_), Element::DenseNode(_) | Element::Node(_)) => {
            write_node_details_borrowed(output, old, new)
        }
        (Element::Way(ow), Element::Way(nw)) => write_way_details_borrowed(output, ow, nw),
        (Element::Relation(or), Element::Relation(nr)) => {
            write_relation_details_borrowed(output, or, nr)
        }
        _ => Ok(()),
    }
}

fn write_node_details_borrowed(
    output: &mut dyn Write,
    old: &Element<'_>,
    new: &Element<'_>,
) -> Result<()> {
    let (o_lat, o_lon) = match old {
        Element::DenseNode(dn) => (dn.decimicro_lat(), dn.decimicro_lon()),
        Element::Node(n) => (n.decimicro_lat(), n.decimicro_lon()),
        _ => return Ok(()),
    };
    let (n_lat, n_lon) = match new {
        Element::DenseNode(dn) => (dn.decimicro_lat(), dn.decimicro_lon()),
        Element::Node(n) => (n.decimicro_lat(), n.decimicro_lon()),
        _ => return Ok(()),
    };
    if o_lat != n_lat || o_lon != n_lon {
        let mut buf = String::new();
        format_coord(&mut buf, from_decimicro(o_lat));
        let old_lat = buf.clone();
        format_coord(&mut buf, from_decimicro(o_lon));
        let old_lon = buf.clone();
        format_coord(&mut buf, from_decimicro(n_lat));
        let new_lat = buf.clone();
        format_coord(&mut buf, from_decimicro(n_lon));
        writeln!(
            output,
            "  coordinates: ({old_lat}, {old_lon}) -> ({new_lat}, {buf})",
        )?;
    }
    write_tag_diff_borrowed(output, old, new)?;
    Ok(())
}

fn write_way_details_borrowed(
    output: &mut dyn Write,
    old: &crate::Way<'_>,
    new: &crate::Way<'_>,
) -> Result<()> {
    let old_refs: Vec<i64> = old.refs().collect();
    let new_refs: Vec<i64> = new.refs().collect();
    write_seq_edit(output, "ref", "refs", &old_refs, &new_refs, |w, id| {
        write!(w, "n{id}")
    })?;
    write_tag_diff_iter(output, old.tags_checked(), new.tags_checked())?;
    Ok(())
}

fn write_relation_details_borrowed(
    output: &mut dyn Write,
    old: &crate::Relation<'_>,
    new: &crate::Relation<'_>,
) -> Result<()> {
    write_member_diff_borrowed(output, old, new)?;
    write_tag_diff_iter(output, old.tags_checked(), new.tags_checked())?;
    Ok(())
}

/// Tag diff using borrowed, checked tag iterators. No String allocation for
/// key/value data.
fn write_tag_diff_iter<'a>(
    output: &mut dyn Write,
    old_tags: impl Iterator<Item = Result<(&'a str, &'a str)>>,
    new_tags: impl Iterator<Item = Result<(&'a str, &'a str)>>,
) -> Result<()> {
    let old_vec: Vec<(&str, &str)> = old_tags.collect::<Result<_>>()?;
    let new_vec: Vec<(&str, &str)> = new_tags.collect::<Result<_>>()?;
    let old_map: HashMap<&str, &str> = old_vec.iter().copied().collect();
    let new_map: HashMap<&str, &str> = new_vec.iter().copied().collect();

    // A duplicate key makes the keyed view ambiguous: fall back to the
    // ordered (key, value) sequence diff, which accounts for every copy.
    if old_map.len() != old_vec.len() || new_map.len() != new_vec.len() {
        return write_seq_edit(output, "tag", "tags", &old_vec, &new_vec, |w, (k, v)| {
            write!(w, "{}={}", Quoted(k), Quoted(v))
        });
    }

    for (k, v) in &old_vec {
        if !new_map.contains_key(k) {
            writeln!(output, "  -{}={}", Quoted(k), Quoted(v))?;
        }
    }
    for (k, v) in &new_vec {
        if !old_map.contains_key(k) {
            writeln!(output, "  +{}={}", Quoted(k), Quoted(v))?;
        }
    }
    for (k, new_v) in &new_vec {
        if let Some(old_v) = old_map.get(k)
            && old_v != new_v
        {
            writeln!(
                output,
                "  ~{}: {} -> {}",
                Quoted(k),
                Quoted(old_v),
                Quoted(new_v)
            )?;
        }
    }
    // Tag order is element content, so a reorder needs a line of its own.
    // Only keys present on both sides are compared, so a plain insertion or
    // removal is not mistaken for a reorder.
    let common_old = old_vec.iter().filter(|(k, _)| new_map.contains_key(k));
    let common_new = new_vec.iter().filter(|(k, _)| old_map.contains_key(k));
    if !common_old.map(|(k, _)| k).eq(common_new.map(|(k, _)| k)) {
        writeln!(output, "  tags: order changed")?;
    }
    Ok(())
}

/// Tag diff dispatching across DenseNode/Node tag iterator types.
fn write_tag_diff_borrowed(
    output: &mut dyn Write,
    old: &Element<'_>,
    new: &Element<'_>,
) -> Result<()> {
    match (old, new) {
        (Element::DenseNode(da), Element::DenseNode(db)) => {
            write_tag_diff_iter(output, da.tags_checked(), db.tags_checked())
        }
        (Element::DenseNode(da), Element::Node(nb)) => {
            write_tag_diff_iter(output, da.tags_checked(), nb.tags_checked())
        }
        (Element::Node(na), Element::DenseNode(db)) => {
            write_tag_diff_iter(output, na.tags_checked(), db.tags_checked())
        }
        (Element::Node(na), Element::Node(nb)) => {
            write_tag_diff_iter(output, na.tags_checked(), nb.tags_checked())
        }
        _ => Ok(()),
    }
}

/// Member diff using borrowed relation references.
fn write_member_diff_borrowed(
    output: &mut dyn Write,
    old: &crate::Relation<'_>,
    new: &crate::Relation<'_>,
) -> Result<()> {
    let old_members: Vec<(crate::MemberId, &str)> = old
        .members()
        .map(|m| m.role().map(|role| (m.id, role)))
        .collect::<Result<_>>()?;
    let new_members: Vec<(crate::MemberId, &str)> = new
        .members()
        .map(|m| m.role().map(|role| (m.id, role)))
        .collect::<Result<_>>()?;

    // Members are content in order with multiplicity, so the detail is an
    // ordered edit script: a move or a dropped duplicate is a -member and/or
    // +member line, never an empty detail.
    write_seq_edit(
        output,
        "member",
        "members",
        &old_members,
        &new_members,
        |w, (id, role)| write!(w, "{} {}", MemberLabel(*id), Quoted(role)),
    )
}

/// Print the ordered edit script between two sequences: every removal as
/// `  -{item} @{old index} {value}`, then every insertion as
/// `  +{item} @{new index} {value}`, positions zero-based into the original
/// old and final new lists. A script too long or too costly to itemise
/// prints one line instead, `  {list}: N -> M, old @a..b -> new @c..d
/// replaced (details omitted)`, ranges half-open; the replacement is
/// faithful but does not claim every item in the ranges differs. Equal
/// sequences print nothing.
fn write_seq_edit<T: Eq>(
    output: &mut dyn Write,
    item: &str,
    list: &str,
    old: &[T],
    new: &[T],
    value: impl Fn(&mut dyn Write, &T) -> std::io::Result<()>,
) -> Result<()> {
    match seq::seq_diff(old, new) {
        seq::SeqEdit::Exact { removed, inserted } => {
            for i in removed {
                write!(output, "  -{item} @{i} ")?;
                value(output, &old[i])?;
                writeln!(output)?;
            }
            for i in inserted {
                write!(output, "  +{item} @{i} ")?;
                value(output, &new[i])?;
                writeln!(output)?;
            }
        }
        seq::SeqEdit::Replaced { old: o, new: n } => writeln!(
            output,
            "  {list}: {} -> {}, old @{}..{} -> new @{}..{} replaced (details omitted)",
            old.len(),
            new.len(),
            o.start,
            o.end,
            n.start,
            n.end,
        )?,
    }
    Ok(())
}
