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

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use crate::osc::write::{
    format_coord, from_decimicro, OwnedMember, OwnedNode, OwnedRelation, OwnedWay,
};
use crate::osc::merge_join::{
    block_pair_merge_phase, merge_join_phase, BlockMergeAction, BlockPairMergeState,
    MergeJoinAction, MergeJoinElement, StreamingBlocks,
};
use super::Result;
use crate::owned::TypeFilter;
use crate::blob_meta::ElemKind;
use crate::{Element, MemberType};

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
pub(crate) fn check_sorted_and_indexed(path: &Path, direct_io: bool) -> Result<(bool, bool)> {
    use crate::blob::BlobKind;

    // Pass 1: read sorted flag from OsmHeader via BlobReader (reads ~1 blob).
    let sorted = {
        let mut blob_reader = crate::blob::BlobReader::open(path, direct_io)?;
        let mut s = false;
        for blob_result in &mut blob_reader {
            let blob = blob_result?;
            match blob.get_type() {
                crate::blob::BlobType::OsmHeader => {
                    let header = blob.to_headerblock()?;
                    s = header.is_sorted();
                }
                _ => break,
            }
        }
        s
    };

    // Pass 2: O(1) header-only probe of the first OsmData blob.
    let mut reader = crate::file_reader::FileReader::open(path, direct_io)?;
    let mut offset = 0u64;
    let mut indexed = false;

    while let Some(info) = crate::read::raw_frame::read_blob_header_only(&mut reader, &mut offset)? {
        if matches!(info.blob_type, BlobKind::OsmData) {
            indexed = info.index.is_some();
            break;
        }
        reader.skip(info.data_size as u64)?;
        offset += info.data_size as u64;
    }

    Ok((sorted, indexed))
}

// `require_sorted_err` is defined in `super::mod` and re-used here.

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Options for the diff command.
pub struct DiffOptions {
    /// Hide unchanged elements (show only created/modified/deleted).
    pub suppress_common: bool,
    /// Show detailed changes for modified elements.
    pub verbose: bool,
    /// Show summary on stderr (left/right/same/different counts).
    pub summary: bool,
    /// Comma-separated type filter (e.g. "node,way").
    pub type_filter: Option<String>,
}

/// Statistics from a diff operation.
#[derive(Debug)]
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
    pub fn print_summary(&self) {
        let total = self.created + self.modified + self.deleted;
        if total == 0 {
            eprintln!("Files are identical ({} common elements)", self.common);
        } else {
            eprintln!(
                "{total} differences: {} created, {} modified, {} deleted ({} common)",
                self.created, self.modified, self.deleted, self.common,
            );
        }
    }

    /// Print osmium-compatible summary to stderr (left/right/same/different).
    pub fn print_osmium_summary(&self) {
        let left = self.common + self.modified + self.deleted;
        let right = self.common + self.modified + self.created;
        let different = self.created + self.modified + self.deleted;
        eprintln!(
            "Summary: left={left} right={right} same={} different={different}",
            self.common,
        );
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
    fn version(&self) -> Option<i32> { self.metadata.as_ref().map(|m| m.version) }
    fn type_char() -> char { 'n' }
}

impl DiffMeta for OwnedWay {
    fn version(&self) -> Option<i32> { self.metadata.as_ref().map(|m| m.version) }
    fn type_char() -> char { 'w' }
}

impl DiffMeta for OwnedRelation {
    fn version(&self) -> Option<i32> { self.metadata.as_ref().map(|m| m.version) }
    fn type_char() -> char { 'r' }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compare two sorted PBF files and write human-readable differences.
///
/// Streams through both files in constant memory (~3 MB overhead per cursor)
/// using pipelined block iterators. Requires both inputs to declare
/// `Sort.Type_then_ID` - returns an actionable error if either is unsorted.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn diff(
    old_path: &Path,
    new_path: &Path,
    output: &mut impl Write,
    options: &DiffOptions,
    direct_io: bool,
) -> Result<DiffStats> {
    let filter = match options.type_filter.as_deref() {
        Some(s) => TypeFilter::parse(s),
        None => TypeFilter::all(),
    };

    // Single-pass: check sorted headers + indexdata from one file open each.
    let (old_sorted, old_indexed) = check_sorted_and_indexed(old_path, direct_io)?;
    let (new_sorted, new_indexed) = check_sorted_and_indexed(new_path, direct_io)?;
    if !old_sorted { super::require_sorted_err(old_path, "Old PBF")?; }
    if !new_sorted { super::require_sorted_err(new_path, "New PBF")?; }
    let both_indexed = old_indexed && new_indexed;

    crate::debug::emit_marker("DIFF_SCAN_START");

    let stats = if both_indexed {
        diff_block_pair(old_path, new_path, output, options, direct_io, &filter)?
    } else {
        diff_element_stream(old_path, new_path, output, options, direct_io, &filter)?
    };

    crate::debug::emit_marker("DIFF_SCAN_END");
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("diff_common", stats.common as i64);
        crate::debug::emit_counter("diff_created", stats.created as i64);
        crate::debug::emit_counter("diff_modified", stats.modified as i64);
        crate::debug::emit_counter("diff_deleted", stats.deleted as i64);
    }

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
) -> Result<DiffStats> {
    let mut old_reader = crate::blob::BlobReader::open(old_path, direct_io)?;
    old_reader.set_parse_indexdata(true);
    let mut new_reader = crate::blob::BlobReader::open(new_path, direct_io)?;
    new_reader.set_parse_indexdata(true);

    let mut merge = BlockPairMergeState::new(old_reader, new_reader);

    let mut stats = DiffStats {
        common: 0,
        created: 0,
        modified: 0,
        deleted: 0,
    };

    let phases: [(ElemKind, bool, &str); 3] = [
        (ElemKind::Node, filter.nodes, "NODE"),
        (ElemKind::Way, filter.ways, "WAY"),
        (ElemKind::Relation, filter.relations, "REL"),
    ];

    for (kind, enabled, tag) in phases {
        if !enabled {
            continue;
        }

        let start_marker = format!("DIFF_PHASE_{tag}_START");
        let end_marker = format!("DIFF_PHASE_{tag}_END");
        crate::debug::emit_marker(&start_marker);

        block_pair_merge_phase(
            &mut merge,
            kind,
            options.suppress_common,
            &mut |action| {
                match action {
                    BlockMergeAction::BlobEqual(count) => {
                        stats.common += count;
                    }
                    BlockMergeAction::BlobOldOnly {
                        block, count, skip,
                    } => {
                        let type_char = crate::osc::merge_join::kind_type_char(kind);
                        for elem in block.elements().skip(skip) {
                            let id = crate::osc::merge_join::element_id(&elem);
                            let ver = crate::osc::merge_join::element_version(&elem);
                            write_compact_line(output, '-', type_char, id, ver)?;
                        }
                        stats.deleted += count;
                    }
                    BlockMergeAction::BlobNewOnly {
                        block, count, skip,
                    } => {
                        let type_char = crate::osc::merge_join::kind_type_char(kind);
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
                        let type_char = crate::osc::merge_join::kind_type_char(kind);
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
                        let type_char = crate::osc::merge_join::kind_type_char(kind);
                        let id = crate::osc::merge_join::element_id(o);
                        let ver = crate::osc::merge_join::element_version(o);
                        write_compact_line(output, '-', type_char, id, ver)?;
                        stats.deleted += 1;
                    }
                    BlockMergeAction::ElementNewOnly(n) => {
                        let type_char = crate::osc::merge_join::kind_type_char(kind);
                        let id = crate::osc::merge_join::element_id(n);
                        let ver = crate::osc::merge_join::element_version(n);
                        write_compact_line(output, '+', type_char, id, ver)?;
                        stats.created += 1;
                    }
                }
                Ok(())
            },
        )?;
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
    crate::debug::emit_counter("mergejoin_shadow_pairs_byte_equal", s.pairs_byte_equal as i64);
    crate::debug::emit_counter("mergejoin_shadow_elements_byte_equal", s.elements_byte_equal as i64);
    crate::debug::emit_counter("mergejoin_shadow_pairs_overlapping_decoded", s.pairs_overlapping_decoded as i64);
    crate::debug::emit_counter("mergejoin_shadow_elements_overlapping_decoded", s.elements_overlapping_decoded as i64);
    crate::debug::emit_counter("mergejoin_shadow_blobs_old_only", s.blobs_old_only as i64);
    crate::debug::emit_counter("mergejoin_shadow_elements_old_only", s.elements_old_only as i64);
    crate::debug::emit_counter("mergejoin_shadow_blobs_new_only", s.blobs_new_only as i64);
    crate::debug::emit_counter("mergejoin_shadow_elements_new_only", s.elements_new_only as i64);
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
) -> Result<DiffStats> {
    let mut old_src = StreamingBlocks::new_sequential(old_path, direct_io)?;
    let mut new_src = StreamingBlocks::new_sequential(new_path, direct_io)?;

    let mut stats = DiffStats {
        common: 0,
        created: 0,
        modified: 0,
        deleted: 0,
    };

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
    let DiffPhaseCtx { output, opts, stats } = ctx;
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
    merge_join_phase::<T>(old_src, &mut Vec::new(), new_src, &mut Vec::new(), |_| Ok(()))
}

// ---------------------------------------------------------------------------
// Output formatting -- compact lines
// ---------------------------------------------------------------------------

fn write_compact_line(
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

fn write_modified_line(
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

fn write_node_details(
    output: &mut dyn Write,
    old: &OwnedNode,
    new: &OwnedNode,
) -> Result<()> {
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

fn write_way_details(
    output: &mut dyn Write,
    old: &OwnedWay,
    new: &OwnedWay,
) -> Result<()> {
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
        if let Some((_, old_v)) = old_tags.iter().find(|(ok, _)| ok == k) {
            if old_v != v {
                writeln!(output, "  ~{k}: {old_v} -> {v}")?;
            }
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
    let new_set: HashSet<(crate::MemberId, &str)> =
        new_members.iter().map(|m| (m.id, m.role.as_str())).collect();
    let old_set: HashSet<(crate::MemberId, &str)> =
        old_members.iter().map(|m| (m.id, m.role.as_str())).collect();

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
fn write_modified_details_borrowed(
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
        if let Some(old_v) = old_map.get(k) {
            if old_v != new_v {
                writeln!(output, "  ~{k}: {old_v} -> {new_v}")?;
            }
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
        (Element::Node(na), Element::Node(nb)) => {
            write_tag_diff_iter(output, na.tags(), nb.tags())
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
