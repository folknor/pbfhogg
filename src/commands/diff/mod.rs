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

// Under the `static-test-hooks` feature, expose the shard-panic hook so
// an integration-test target of its own can arm it. The rest of `parallel` stays
// crate-private.
#[cfg(all(feature = "static-test-hooks", not(test)))]
pub mod parallel_test_hooks {
    pub use super::parallel::test_hooks::{PANIC_AT_SHARD_IDX, reset};
}

#[cfg(all(feature = "static-test-hooks", not(test)))]
pub mod derive_parallel_test_hooks {
    pub use super::derive_parallel::test_hooks::{PANIC_AT_SHARD_IDX, reset};
}

use super::Result;
use crate::ElementKind;
use crate::coord_fmt::{format_coord, from_decimicro};
use crate::osc::merge_join::{
    BlockMergeAction, BlockPairMergeState, MergeJoinAction, MergeJoinElement, StreamingBlocks,
    block_pair_merge_phase, merge_join_phase,
};
use crate::osc::write::{OwnedMember, OwnedNode, OwnedRelation, OwnedWay};
use crate::owned::TypeFilter;
use crate::{Element, MemberType};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a PBF is sorted and has indexdata.
/// Returns `(is_sorted, has_indexdata)`.
///
/// The sorted flag comes from the OsmHeader blob (first blob, needs full
/// read + decompress). Indexdata is probed from the first OsmData blob
/// only - O(1) header read. Partially-indexed PBFs surface as a mid-run
/// error at the consuming site rather than being detected up front.
pub(crate) fn check_sorted_and_indexed(
    path: &Path,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<(bool, bool)> {
    // Pass 1: read the sorted flag from the OsmHeader (reads ~1 blob). A
    // file without a leading header is the missing-header error here, ahead
    // of any question about sortedness.
    let sorted = crate::ElementReader::open_in(path, direct_io, host)?
        .header()
        .is_sorted();

    // Pass 2: O(1) header-only probe of the first OsmData blob. A file with
    // no data blob counts as indexed.
    let indexed = super::has_indexdata(path, direct_io, &host.diagnostics)?;

    Ok((sorted, indexed))
}

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
    /// Select the osmium-style summary (left/right/same/different counts).
    /// The caller renders it with `DiffStats::write_osmium_summary`.
    pub summary: bool,
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
// Per-type diff helpers
// ---------------------------------------------------------------------------

trait DiffMeta {
    fn version(&self) -> Option<i32>;
    fn type_char() -> char;
}

impl DiffMeta for OwnedNode {
    fn version(&self) -> Option<i32> {
        self.metadata.as_ref().and_then(|m| m.version)
    }
    fn type_char() -> char {
        'n'
    }
}

impl DiffMeta for OwnedWay {
    fn version(&self) -> Option<i32> {
        self.metadata.as_ref().and_then(|m| m.version)
    }
    fn type_char() -> char {
        'w'
    }
}

impl DiffMeta for OwnedRelation {
    fn version(&self) -> Option<i32> {
        self.metadata.as_ref().and_then(|m| m.version)
    }
    fn type_char() -> char {
        'r'
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
/// its shard files and removed when the call returns.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn diff(
    old_path: &Path,
    new_path: &Path,
    output: &mut impl Write,
    options: &DiffOptions,
    direct_io: bool,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<DiffStats> {
    host.cancel.check()?;
    if options.verbose && options.parallelism.fixed().is_some_and(|n| n > 1) {
        return Err(crate::Error::invalid_argument(
            "diff: a fixed shard count above one has no effect with verbose, whose \
             per-field detail lines are produced by the sequential path only",
        ));
    }
    let jobs = options.shards(host);
    let filter = TypeFilter::or_all(options.type_filter);

    // Single-pass: check sorted headers + indexdata from one file open each.
    let (old_sorted, old_indexed) = check_sorted_and_indexed(old_path, direct_io, host)?;
    let (new_sorted, new_indexed) = check_sorted_and_indexed(new_path, direct_io, host)?;
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
    let parallel_stats = if both_indexed && jobs > 1 {
        parallel::diff_block_pair_parallel(
            old_path, new_path, output, options, direct_io, &filter, jobs, host, &scratch,
        )?
    } else {
        None
    };
    let stats = if let Some(stats) = parallel_stats {
        stats
    } else if both_indexed {
        diff_block_pair(
            old_path, new_path, output, options, direct_io, &filter, host,
        )?
    } else {
        diff_element_stream(
            old_path, new_path, output, options, direct_io, &filter, host,
        )?
    };

    crate::debug::emit_marker("DIFF_SCAN_END");
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("diff_common", stats.common as i64);
        crate::debug::emit_counter("diff_created", stats.created as i64);
        crate::debug::emit_counter("diff_modified", stats.modified as i64);
        crate::debug::emit_counter("diff_deleted", stats.deleted as i64);
    }

    drop(scratch);
    // The completion decision: a cancellation observed by now is reported
    // as one, whatever reached the caller's writer.
    host.cancel.check()?;
    Ok(stats)
}

/// Optimized diff path using block-pair merge with borrowed elements.
/// Requires both inputs to have indexdata. Zero String allocation for
/// unchanged elements (98.8%+ of typical daily diffs).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn diff_block_pair(
    old_path: &Path,
    new_path: &Path,
    output: &mut impl Write,
    options: &DiffOptions,
    direct_io: bool,
    filter: &TypeFilter,
    host: &crate::host::Host,
) -> Result<DiffStats> {
    let mut old_reader = crate::blob::BlobReader::open_in(old_path, direct_io, host)?;
    old_reader.set_parse_indexdata(true);
    let mut new_reader = crate::blob::BlobReader::open_in(new_path, direct_io, host)?;
    new_reader.set_parse_indexdata(true);

    let mut merge = BlockPairMergeState::new(old_reader, new_reader);

    let mut stats = DiffStats::default();

    let phases: [(ElementKind, bool, &str); 3] = [
        (ElementKind::Node, filter.nodes, "NODE"),
        (ElementKind::Way, filter.ways, "WAY"),
        (ElementKind::Relation, filter.relations, "REL"),
    ];

    for (kind, enabled, tag) in phases {
        if !enabled {
            continue;
        }

        let start_marker = format!("DIFF_PHASE_{tag}_START");
        let end_marker = format!("DIFF_PHASE_{tag}_END");
        crate::debug::emit_marker(&start_marker);

        block_pair_merge_phase(&mut merge, kind, options.suppress_common, &mut |action| {
            match action {
                BlockMergeAction::BlobEqual(count) => {
                    stats.common += count;
                }
                BlockMergeAction::BlobOldOnly { block, count, skip } => {
                    let type_char = kind.type_char();
                    for elem in block.elements().skip(skip) {
                        let id = crate::osc::merge_join::element_id(&elem);
                        let ver = crate::osc::merge_join::element_version(&elem);
                        write_compact_line(output, '-', type_char, id, ver)?;
                    }
                    stats.deleted += count;
                }
                BlockMergeAction::BlobNewOnly { block, count, skip } => {
                    let type_char = kind.type_char();
                    for elem in block.elements().skip(skip) {
                        let id = crate::osc::merge_join::element_id(&elem);
                        let ver = crate::osc::merge_join::element_version(&elem);
                        write_compact_line(output, '+', type_char, id, ver)?;
                    }
                    stats.created += count;
                }
                BlockMergeAction::ElementEqual {
                    id,
                    version,
                    type_char,
                } => {
                    if !options.suppress_common {
                        write_compact_line(output, ' ', type_char, id, version)?;
                    }
                    stats.common += 1;
                }
                BlockMergeAction::ElementModified { old, new } => {
                    let type_char = kind.type_char();
                    let id = crate::osc::merge_join::element_id(old);
                    let old_ver = crate::osc::merge_join::element_version(old);
                    let new_ver = crate::osc::merge_join::element_version(new);
                    write_modified_line(output, type_char, id, old_ver, new_ver)?;
                    if options.verbose {
                        write_modified_details_borrowed(output, old, new)?;
                    }
                    stats.modified += 1;
                }
                BlockMergeAction::ElementOldOnly(o) => {
                    let type_char = kind.type_char();
                    let id = crate::osc::merge_join::element_id(o);
                    let ver = crate::osc::merge_join::element_version(o);
                    write_compact_line(output, '-', type_char, id, ver)?;
                    stats.deleted += 1;
                }
                BlockMergeAction::ElementNewOnly(n) => {
                    let type_char = kind.type_char();
                    let id = crate::osc::merge_join::element_id(n);
                    let ver = crate::osc::merge_join::element_version(n);
                    write_compact_line(output, '+', type_char, id, ver)?;
                    stats.created += 1;
                }
            }
            Ok(())
        })?;
        crate::debug::emit_marker(&end_marker);
    }

    emit_merge_stats_counters(&merge.stats);
    Ok(stats)
}

/// Emit `mergejoin_shadow_*` counters accumulated across all phases.
/// Shadow-only - no code-path change. See `BlockPairMergeStats` and the
/// diff-snapshots opportunity plan for how these feed the v3 decision.
#[allow(clippy::cast_possible_wrap)]
fn emit_merge_stats_counters(s: &crate::osc::merge_join::BlockPairMergeStats) {
    crate::debug::emit_counter(
        "mergejoin_shadow_pairs_byte_equal",
        s.pairs_byte_equal as i64,
    );
    crate::debug::emit_counter(
        "mergejoin_shadow_elements_byte_equal",
        s.elements_byte_equal as i64,
    );
    crate::debug::emit_counter(
        "mergejoin_shadow_pairs_overlapping_decoded",
        s.pairs_overlapping_decoded as i64,
    );
    crate::debug::emit_counter(
        "mergejoin_shadow_elements_overlapping_decoded",
        s.elements_overlapping_decoded as i64,
    );
    crate::debug::emit_counter("mergejoin_shadow_blobs_old_only", s.blobs_old_only as i64);
    crate::debug::emit_counter(
        "mergejoin_shadow_elements_old_only",
        s.elements_old_only as i64,
    );
    crate::debug::emit_counter("mergejoin_shadow_blobs_new_only", s.blobs_new_only as i64);
    crate::debug::emit_counter(
        "mergejoin_shadow_elements_new_only",
        s.elements_new_only as i64,
    );
}

/// Fallback diff path using element-level merge-join with owned elements.
/// Used when either input lacks indexdata.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn diff_element_stream(
    old_path: &Path,
    new_path: &Path,
    output: &mut impl Write,
    options: &DiffOptions,
    direct_io: bool,
    filter: &TypeFilter,
    host: &crate::host::Host,
) -> Result<DiffStats> {
    let mut old_src = StreamingBlocks::new_sequential(old_path, direct_io, host)?;
    let mut new_src = StreamingBlocks::new_sequential(new_path, direct_io, host)?;

    let mut stats = DiffStats::default();

    if filter.nodes {
        crate::debug::emit_marker("DIFF_PHASE_NODE_START");
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        let mut ctx = DiffPhaseCtx {
            output,
            opts: options,
            stats: &mut stats,
        };
        run_diff_phase(
            &mut old_src,
            &mut ob,
            &mut new_src,
            &mut nb,
            &mut ctx,
            write_node_details,
        )?;
        crate::debug::emit_marker("DIFF_PHASE_NODE_END");
    } else {
        drain_phase::<OwnedNode>(&mut old_src, &mut new_src)?;
    }

    if filter.ways {
        crate::debug::emit_marker("DIFF_PHASE_WAY_START");
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        let mut ctx = DiffPhaseCtx {
            output,
            opts: options,
            stats: &mut stats,
        };
        run_diff_phase(
            &mut old_src,
            &mut ob,
            &mut new_src,
            &mut nb,
            &mut ctx,
            write_way_details,
        )?;
        crate::debug::emit_marker("DIFF_PHASE_WAY_END");
    } else {
        drain_phase::<OwnedWay>(&mut old_src, &mut new_src)?;
    }

    if filter.relations {
        crate::debug::emit_marker("DIFF_PHASE_REL_START");
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        let mut ctx = DiffPhaseCtx {
            output,
            opts: options,
            stats: &mut stats,
        };
        run_diff_phase(
            &mut old_src,
            &mut ob,
            &mut new_src,
            &mut nb,
            &mut ctx,
            write_relation_details,
        )?;
        crate::debug::emit_marker("DIFF_PHASE_REL_END");
    } else {
        drain_phase::<OwnedRelation>(&mut old_src, &mut new_src)?;
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Diff phase wrappers over generic merge-join
// ---------------------------------------------------------------------------

/// Context for a single diff phase (avoids too-many-arguments lint).
struct DiffPhaseCtx<'a, W: Write> {
    output: &'a mut W,
    opts: &'a DiffOptions,
    stats: &'a mut DiffStats,
}

/// Run one diff phase using the shared merge-join, emitting output immediately.
fn run_diff_phase<T: MergeJoinElement + DiffMeta>(
    old_src: &mut StreamingBlocks,
    old_buf: &mut Vec<T>,
    new_src: &mut StreamingBlocks,
    new_buf: &mut Vec<T>,
    ctx: &mut DiffPhaseCtx<'_, impl Write>,
    write_details: fn(&mut dyn Write, &T, &T) -> Result<()>,
) -> Result<()> {
    let DiffPhaseCtx {
        output,
        opts,
        stats,
    } = ctx;
    merge_join_phase(old_src, old_buf, new_src, new_buf, |action| {
        match action {
            MergeJoinAction::OldOnly(o) => {
                write_compact_line(output, '-', T::type_char(), o.id(), o.version())?;
                stats.deleted += 1;
            }
            MergeJoinAction::NewOnly(n) => {
                write_compact_line(output, '+', T::type_char(), n.id(), n.version())?;
                stats.created += 1;
            }
            MergeJoinAction::Modified(o, n) => {
                write_modified_line(output, T::type_char(), o.id(), o.version(), n.version())?;
                if opts.verbose {
                    write_details(output, o, n)?;
                }
                stats.modified += 1;
            }
            MergeJoinAction::Equal(o) => {
                if !opts.suppress_common {
                    write_compact_line(output, ' ', T::type_char(), o.id(), o.version())?;
                }
                stats.common += 1;
            }
        }
        Ok(())
    })
}

/// Drain remaining elements of type `T` from both cursors without processing.
///
/// Called to advance past a skipped phase (e.g. when type_filter excludes
/// nodes) so the cursors are positioned for the next phase.
fn drain_phase<T: MergeJoinElement>(
    old_src: &mut StreamingBlocks,
    new_src: &mut StreamingBlocks,
) -> Result<()> {
    merge_join_phase::<T>(old_src, &mut Vec::new(), new_src, &mut Vec::new(), |_| {
        Ok(())
    })
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
// Output formatting -- verbose details
// ---------------------------------------------------------------------------

fn write_node_details(output: &mut dyn Write, old: &OwnedNode, new: &OwnedNode) -> Result<()> {
    if old.decimicro_lat != new.decimicro_lat || old.decimicro_lon != new.decimicro_lon {
        let mut buf = String::new();
        format_coord(&mut buf, from_decimicro(old.decimicro_lat));
        let old_lat = buf.clone();
        format_coord(&mut buf, from_decimicro(old.decimicro_lon));
        let old_lon = buf.clone();
        format_coord(&mut buf, from_decimicro(new.decimicro_lat));
        let new_lat = buf.clone();
        format_coord(&mut buf, from_decimicro(new.decimicro_lon));
        writeln!(
            output,
            "  coordinates: ({old_lat}, {old_lon}) -> ({new_lat}, {buf})",
        )?;
    }
    write_tag_diff(output, &old.tags, &new.tags)?;
    Ok(())
}

fn write_way_details(output: &mut dyn Write, old: &OwnedWay, new: &OwnedWay) -> Result<()> {
    if old.refs != new.refs {
        writeln!(
            output,
            "  refs: {} -> {} nodes",
            old.refs.len(),
            new.refs.len(),
        )?;
    }
    write_tag_diff(output, &old.tags, &new.tags)?;
    Ok(())
}

fn write_relation_details(
    output: &mut dyn Write,
    old: &OwnedRelation,
    new: &OwnedRelation,
) -> Result<()> {
    write_member_diff(output, &old.members, &new.members)?;
    write_tag_diff(output, &old.tags, &new.tags)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tag diff
// ---------------------------------------------------------------------------

fn write_tag_diff(
    output: &mut dyn Write,
    old_tags: &[(String, String)],
    new_tags: &[(String, String)],
) -> Result<()> {
    // Linear scan - elements typically have 2-5 tags, faster than HashMap.
    // Removed: in old but not in new
    for (k, v) in old_tags {
        if !new_tags.iter().any(|(nk, _)| nk == k) {
            writeln!(output, "  -{k}={v}")?;
        }
    }
    // Added: in new but not in old
    for (k, v) in new_tags {
        if !old_tags.iter().any(|(ok, _)| ok == k) {
            writeln!(output, "  +{k}={v}")?;
        }
    }
    // Changed: same key, different value
    for (k, v) in new_tags {
        if let Some((_, old_v)) = old_tags.iter().find(|(ok, _)| ok == k)
            && old_v != v
        {
            writeln!(output, "  ~{k}: {old_v} -> {v}")?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Member diff
// ---------------------------------------------------------------------------

fn member_type_str(mt: MemberType) -> &'static str {
    match mt {
        MemberType::Node => "node",
        MemberType::Way => "way",
        MemberType::Relation => "relation",
        MemberType::Unknown(_) => "unknown",
    }
}

fn write_member_diff(
    output: &mut dyn Write,
    old_members: &[OwnedMember],
    new_members: &[OwnedMember],
) -> Result<()> {
    let new_set: HashSet<(crate::MemberId, &str)> = new_members
        .iter()
        .map(|m| (m.id, m.role.as_str()))
        .collect();
    let old_set: HashSet<(crate::MemberId, &str)> = old_members
        .iter()
        .map(|m| (m.id, m.role.as_str()))
        .collect();

    // Removed members (in old but not in new)
    for old_m in old_members {
        if !new_set.contains(&(old_m.id, old_m.role.as_str())) {
            writeln!(
                output,
                "  -member {}/{} \"{}\"",
                member_type_str(old_m.id.member_type()),
                old_m.id.id(),
                old_m.role,
            )?;
        }
    }
    // Added members (in new but not in old)
    for new_m in new_members {
        if !old_set.contains(&(new_m.id, new_m.role.as_str())) {
            writeln!(
                output,
                "  +member {}/{} \"{}\"",
                member_type_str(new_m.id.member_type()),
                new_m.id.id(),
                new_m.role,
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Borrowed-element verbose details (block-pair path)
// ---------------------------------------------------------------------------

/// Write verbose modification details using borrowed element references.
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
    write_tag_diff_iter(output, old.tags(), new.tags())?;
    Ok(())
}

fn write_relation_details_borrowed(
    output: &mut dyn Write,
    old: &crate::Relation<'_>,
    new: &crate::Relation<'_>,
) -> Result<()> {
    write_member_diff_borrowed(output, old, new)?;
    write_tag_diff_iter(output, old.tags(), new.tags())?;
    Ok(())
}

/// Tag diff using borrowed tag iterators. No String allocation for key/value data.
fn write_tag_diff_iter<'a>(
    output: &mut dyn Write,
    old_tags: impl Iterator<Item = (&'a str, &'a str)>,
    new_tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<()> {
    let old_vec: Vec<(&str, &str)> = old_tags.collect();
    let new_vec: Vec<(&str, &str)> = new_tags.collect();
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
            write_tag_diff_iter(output, da.tags(), db.tags())
        }
        (Element::DenseNode(da), Element::Node(nb)) => {
            write_tag_diff_iter(output, da.tags(), nb.tags())
        }
        (Element::Node(na), Element::DenseNode(db)) => {
            write_tag_diff_iter(output, na.tags(), db.tags())
        }
        (Element::Node(na), Element::Node(nb)) => write_tag_diff_iter(output, na.tags(), nb.tags()),
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
        .map(|m| (m.id, m.role().unwrap_or("")))
        .collect();
    let new_members: Vec<(crate::MemberId, &str)> = new
        .members()
        .map(|m| (m.id, m.role().unwrap_or("")))
        .collect();

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
