//! Compare two PBF files and output human-readable differences. Equivalent to `osmium diff`.
//!
//! Streams through both files in constant memory using [`StreamingBlocks`] cursors.
//! Requires both inputs to declare `Sort.Type_then_ID`.
//!
//! # Design: content equality, not version ordering
//!
//! Unlike osmium's diff, which uses a version/timestamp comparator to order elements and
//! can produce wrong output when inputs have mismatched metadata (osmium-tool#93), pbfhogg
//! diff uses content equality: two elements with the same type+ID are compared field by
//! field (coordinates, tags, refs, members). This makes diff output deterministic regardless
//! of whether metadata (version, timestamp, changeset, uid) is present, partial, or absent.

pub mod derive;
pub(crate) mod derive_parallel;
pub(crate) mod parallel;
mod shard;

use super::Result;
use super::input_front::sorted_and_indexed;
use crate::ElementKind;
use crate::coord_fmt::{format_coord, from_decimicro};
use crate::osc::merge_join::{
    BlockMergeAction, BlockPairMergeState, StreamingBlocks, block_pair_merge_phase, skip_phase,
    stream_merge_phase, verify_rest,
};
use crate::owned::TypeFilter;
use crate::read::input::{InputSource, PinnedInput};
use crate::{Element, MemberType};
use std::collections::{HashMap, HashSet};
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
    /// Number of parallel shards (and worker threads) for the block-pair
    /// merge. `Fixed(1)` runs the sequential path; `Fixed(n)` dispatches `n`
    /// shards across worker threads; `Auto` uses one shard per core the host
    /// reports (one when it reports none). `verbose` is sequential-only, so
    /// a fixed count above one is refused alongside it, and `Auto` with
    /// `verbose` runs sequentially.
    pub parallelism: crate::host::Parallelism,
}

impl DiffOptions {
    /// The shard count this run uses: the fixed count, else one per core.
    fn shards(&self, host: &crate::host::Host) -> usize {
        if self.verbose {
            return 1;
        }
        resolve_shards(self.parallelism, host)
    }
}

/// The derive (`--format osc`) shard count: the fixed count, else one per
/// core the host reports (one when it reports none). The same rule as
/// [`DiffOptions`] without the verbose case.
pub(crate) fn resolve_shards(
    parallelism: crate::host::Parallelism,
    host: &crate::host::Host,
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
/// Streams through both files in constant memory (~3 MB overhead per cursor)
/// using pipelined block iterators. Requires both inputs to declare
/// `Sort.Type_then_ID` - returns an actionable error if either is unsorted.
///
/// `scratch` is the run's scratch directory, used by the parallel path for
/// its shard files and removed when the call returns. A caller whose options
/// run one shard (`Fixed(1)`, or `verbose`, which is sequential-only) writes
/// no scratch and passes `None`; `None` with more than one shard is refused,
/// before any input is read. A caller that wants only the counts calls
/// [`diff_stats`], which shards without scratch.
///
/// Every route opens each input more than once, so both must be regular
/// files or block devices (`InvalidArgument` otherwise) and are pinned for
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
    host.cancel.check()?;
    if options.verbose && options.parallelism.fixed().is_some_and(|n| n > 1) {
        return Err(crate::Error::invalid_argument(
            "diff: a fixed shard count above one has no effect with verbose, whose \
             per-field detail lines are produced by the sequential path only",
        ));
    }
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
    host.cancel.check()?;
    Ok(stats)
}

/// Count the differences between two sorted PBF files without producing
/// any text.
///
/// The comparison is the one [`diff`] runs with `suppress_common` and
/// `verbose` off and the same `type_filter` and `parallelism`: the same
/// paths (parallel shards for indexed inputs, the sequential fallbacks
/// otherwise), the same input checks and refusals, and on success the same
/// counts. Only the formatting is skipped, so no output or scratch I/O
/// happens at all, and a sharded comparison needs no scratch directory.
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
    host.cancel.check()?;
    let options = DiffOptions {
        suppress_common: false,
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
    host.cancel.check()?;
    Ok(stats)
}

/// Pin both inputs, `old` first, before either is read: a path spelled
/// exactly as the other shares its pin, a caller's pin is used as it is
/// (see `read::input::pin_sources`).
fn pin_pair(
    old: InputSource<'_>,
    new: InputSource<'_>,
    host: &crate::host::Host,
) -> Result<[PinnedInput; 2]> {
    let pins = crate::read::input::pin_sources(&[old, new], host)?;
    <[PinnedInput; 2]>::try_from(pins)
        .map_err(|_| crate::Error::internal("diff: two inputs must yield two pins"))
}

/// What a comparison produces besides its counts.
enum Report<'a, W> {
    /// Diff text written to `out`. `scratch` holds the parallel path's shard
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
    host: &crate::host::Host,
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
        "Streaming diff requires sorted inputs to operate in constant memory.",
    )?;
    super::require_sorted(
        new_sorted,
        new_path,
        "New PBF",
        "Streaming diff requires sorted inputs to operate in constant memory.",
    )?;
    let both_indexed = old_indexed && new_indexed;
    if jobs > 1 && !both_indexed {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "diff.sequential_fallback",
            "Using sequential comparison: missing indexdata"
        );
        crate::debug::emit_counter("diff_sequential_fallback", 1);
    }

    crate::debug::emit_marker("DIFF_SCAN_START");

    // Verbose always takes the sequential path: the parallel shard
    // workers emit summary lines only (per-field modification details
    // are not implemented there), and silently losing detail lines
    // would be worse than the wall-time cost of a sequential verbose
    // diff. Verbose is a human-inspection mode; the parallel default
    // serves the common non-verbose case.
    //
    // Path selection depends on the inputs and the shard count only, never on
    // whether text is wanted: a count-only run shards exactly as a text run.
    let output = match report {
        Report::Text { out, scratch } => {
            if both_indexed && jobs > 1 {
                // `diff` refuses a multi-shard run without scratch before
                // any input is read.
                let scratch = scratch.ok_or_else(|| {
                    crate::Error::invalid_argument(
                        "diff: a run that may use more than one shard needs a scratch directory",
                    )
                })?;
                if let Some(stats) = parallel::diff_block_pair_parallel(
                    old,
                    new,
                    Some(parallel::ShardText {
                        out: &mut *out,
                        scratch,
                    }),
                    options,
                    direct_io,
                    &filter,
                    jobs,
                    host,
                )? {
                    return Ok(finish_scan(stats));
                }
            }
            Some(out)
        }
        Report::Count => {
            if both_indexed
                && jobs > 1
                && let Some(stats) = parallel::diff_block_pair_parallel::<W>(
                    old, new, None, options, direct_io, &filter, jobs, host,
                )?
            {
                return Ok(finish_scan(stats));
            }
            None
        }
    };
    let stats = if both_indexed {
        diff_block_pair(old, new, output, options, direct_io, &filter, host)?
    } else {
        diff_element_stream(old, new, output, options, direct_io, &filter, host)?
    };
    Ok(finish_scan(stats))
}

/// Close the scan: the end marker and the result counters, on every path.
fn finish_scan(stats: DiffStats) -> DiffStats {
    crate::debug::emit_marker("DIFF_SCAN_END");
    crate::debug::emit_counter("diff_common", stats.common);
    crate::debug::emit_counter("diff_created", stats.created);
    crate::debug::emit_counter("diff_modified", stats.modified);
    crate::debug::emit_counter("diff_deleted", stats.deleted);
    stats
}

/// Optimized diff path using block-pair merge with borrowed elements.
/// Requires both inputs to have indexdata. Zero String allocation for
/// unchanged elements (98.8%+ of typical daily diffs).
///
/// `output` is `None` for a count-only run: every block is still read,
/// decoded and merged exactly as for text (the byte-equal fast path follows
/// `suppress_common`, not the absence of text), only nothing is formatted.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn diff_block_pair<W: Write>(
    old: &PinnedInput,
    new: &PinnedInput,
    output: Option<&mut W>,
    options: &DiffOptions,
    direct_io: bool,
    filter: &TypeFilter,
    host: &crate::host::Host,
) -> Result<DiffStats> {
    let mut old_reader = crate::blob::BlobReader::open_in(old, direct_io, host)?;
    old_reader.set_parse_indexdata(true);
    let mut new_reader = crate::blob::BlobReader::open_in(new, direct_io, host)?;
    new_reader.set_parse_indexdata(true);

    let mut merge = BlockPairMergeState::new(old_reader, new_reader);

    let mut stats = DiffStats::default();
    let mut sink = DiffSink {
        output,
        options,
        stats: &mut stats,
    };

    let phases: [(ElementKind, bool, &str); 3] = [
        (ElementKind::Node, filter.nodes, "NODE"),
        (ElementKind::Way, filter.ways, "WAY"),
        (ElementKind::Relation, filter.relations, "REL"),
    ];

    // An excluded kind ahead of an enabled one is skipped undecoded, so the
    // later phase starts at its own first blob; see `skip_phase`. Excluded
    // kinds after the last enabled one get a header-only walk, `verify_rest`,
    // after the loop.
    let last_enabled = phases.iter().rposition(|&(_, enabled, _)| enabled);
    for (i, (kind, enabled, tag)) in phases.into_iter().enumerate() {
        if !enabled {
            if last_enabled.is_some_and(|last| i < last) {
                skip_phase(&mut merge, kind)?;
            }
            continue;
        }

        let start_marker = format!("DIFF_PHASE_{tag}_START");
        let end_marker = format!("DIFF_PHASE_{tag}_END");
        crate::debug::emit_marker(&start_marker);

        block_pair_merge_phase(&mut merge, kind, options.suppress_common, &mut |action| {
            sink.record(kind, &action)
        })?;
        crate::debug::emit_marker(&end_marker);
    }

    // The structure the parallel path's header walk checks over the whole
    // of both files, checked here for the part no phase read.
    if last_enabled.is_none_or(|last| last + 1 < phases.len()) {
        verify_rest(&mut merge, old.into(), new.into(), host)?;
    }

    emit_merge_stats_counters(&merge.stats);
    Ok(stats)
}

/// Where a sequential diff's merge actions go: the counts, and the text when
/// there is an output. Shared by both sequential paths, so a difference
/// prints the same line (and, under `verbose`, the same details) whether or
/// not the inputs carried indexdata.
struct DiffSink<'a, W: Write> {
    /// `None` for a count-only run.
    output: Option<&'a mut W>,
    options: &'a DiffOptions,
    stats: &'a mut DiffStats,
}

impl<W: Write> DiffSink<'_, W> {
    fn record(&mut self, kind: ElementKind, action: &BlockMergeAction<'_>) -> Result<()> {
        use crate::osc::merge_join::{element_id, element_version};
        let type_char = kind.type_char();
        match *action {
            BlockMergeAction::BlobEqual(count) => {
                self.stats.common += count;
            }
            // A single-sided block's count is the claim the merge already
            // decoded against; count-only runs skip the element walk
            // that exists only to format each line.
            BlockMergeAction::BlobOldOnly { block, count, skip } => {
                if let Some(out) = self.output.as_deref_mut() {
                    for elem in block.elements().skip(skip) {
                        let ver = element_version(&elem);
                        write_compact_line(out, '-', type_char, element_id(&elem), ver)?;
                    }
                }
                self.stats.deleted += count;
            }
            BlockMergeAction::BlobNewOnly { block, count, skip } => {
                if let Some(out) = self.output.as_deref_mut() {
                    for elem in block.elements().skip(skip) {
                        let ver = element_version(&elem);
                        write_compact_line(out, '+', type_char, element_id(&elem), ver)?;
                    }
                }
                self.stats.created += count;
            }
            BlockMergeAction::ElementEqual {
                id,
                version,
                type_char,
            } => {
                if !self.options.suppress_common
                    && let Some(out) = self.output.as_deref_mut()
                {
                    write_compact_line(out, ' ', type_char, id, version)?;
                }
                self.stats.common += 1;
            }
            BlockMergeAction::ElementModified { old, new } => {
                if let Some(out) = self.output.as_deref_mut() {
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
                if let Some(out) = self.output.as_deref_mut() {
                    write_compact_line(out, '-', type_char, element_id(o), element_version(o))?;
                }
                self.stats.deleted += 1;
            }
            BlockMergeAction::ElementNewOnly(n) => {
                if let Some(out) = self.output.as_deref_mut() {
                    write_compact_line(out, '+', type_char, element_id(n), element_version(n))?;
                }
                self.stats.created += 1;
            }
        }
        Ok(())
    }
}

/// Emit `mergejoin_shadow_*` counters accumulated across all phases.
/// Shadow-only - no code-path change. Shared by the text diff and the
/// derive (`--format osc`) sequential block-pair paths, which both drive
/// `BlockPairMergeState` and report the same shadow view.
fn emit_merge_stats_counters(s: &crate::osc::merge_join::BlockPairMergeStats) {
    crate::debug::emit_counter("mergejoin_shadow_pairs_byte_equal", s.pairs_byte_equal);
    crate::debug::emit_counter(
        "mergejoin_shadow_elements_byte_equal",
        s.elements_byte_equal,
    );
    crate::debug::emit_counter(
        "mergejoin_shadow_pairs_overlapping_decoded",
        s.pairs_overlapping_decoded,
    );
    crate::debug::emit_counter(
        "mergejoin_shadow_elements_overlapping_decoded",
        s.elements_overlapping_decoded,
    );
    crate::debug::emit_counter("mergejoin_shadow_blobs_old_only", s.blobs_old_only);
    crate::debug::emit_counter("mergejoin_shadow_elements_old_only", s.elements_old_only);
    crate::debug::emit_counter("mergejoin_shadow_blobs_new_only", s.blobs_new_only);
    crate::debug::emit_counter("mergejoin_shadow_elements_new_only", s.elements_new_only);
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
    host: &crate::host::Host,
) -> Result<DiffStats> {
    let pool = crate::osc::merge_join::merge_buffers();
    let mut old_src =
        StreamingBlocks::new_sequential(old, direct_io, std::sync::Arc::clone(&pool), host)?;
    let mut new_src = StreamingBlocks::new_sequential(new, direct_io, pool, host)?;

    let mut stats = DiffStats::default();
    let mut sink = DiffSink {
        output,
        options,
        stats: &mut stats,
    };

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
            crate::debug::emit_marker(start);
            stream_merge_phase(&mut old_src, &mut new_src, kind, &mut |action| {
                sink.record(kind, &action)
            })?;
            crate::debug::emit_marker(end);
        } else {
            skip_kind(&mut old_src, &mut new_src, kind)?;
        }
    }

    Ok(stats)
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

fn member_type_str(mt: MemberType) -> &'static str {
    match mt {
        MemberType::Node => "node",
        MemberType::Way => "way",
        MemberType::Relation => "relation",
        MemberType::Unknown(_) => "unknown",
    }
}

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
    if old_refs != new_refs {
        writeln!(
            output,
            "  refs: {} -> {} nodes",
            old_refs.len(),
            new_refs.len(),
        )?;
    }
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

    for (k, v) in &old_vec {
        if !new_map.contains_key(k) {
            writeln!(output, "  -{k}={v}")?;
        }
    }
    for (k, v) in &new_vec {
        if !old_map.contains_key(k) {
            writeln!(output, "  +{k}={v}")?;
        }
    }
    for (k, new_v) in &new_vec {
        if let Some(old_v) = old_map.get(k)
            && old_v != new_v
        {
            writeln!(output, "  ~{k}: {old_v} -> {new_v}")?;
        }
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

    let new_set: HashSet<(crate::MemberId, &str)> = new_members.iter().copied().collect();
    let old_set: HashSet<(crate::MemberId, &str)> = old_members.iter().copied().collect();

    for (id, role) in &old_members {
        if !new_set.contains(&(*id, *role)) {
            writeln!(
                output,
                "  -member {}/{} \"{}\"",
                member_type_str(id.member_type()),
                id.id(),
                role,
            )?;
        }
    }
    for (id, role) in &new_members {
        if !old_set.contains(&(*id, *role)) {
            writeln!(
                output,
                "  +member {}/{} \"{}\"",
                member_type_str(id.member_type()),
                id.id(),
                role,
            )?;
        }
    }
    Ok(())
}
