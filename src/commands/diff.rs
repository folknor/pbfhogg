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

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use super::elements_xml::{
    format_coord, from_decimicro, OwnedMember, OwnedNode, OwnedRelation, OwnedWay,
};
use super::stream_merge::{
    block_pair_merge_phase, merge_join_phase, BlockMergeAction, BlockPairMergeState,
    MergeJoinAction, MergeJoinElement, StreamingBlocks,
};
use super::{has_indexdata, require_sorted, Result, TypeFilter};
use crate::blob_index::ElemKind;
use crate::{Element, ElementReader, MemberType};

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
    /// Ignore changeset metadata when comparing.
    pub ignore_changeset: bool,
    /// Ignore uid metadata when comparing.
    pub ignore_uid: bool,
    /// Ignore user metadata when comparing.
    pub ignore_user: bool,
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
/// `Sort.Type_then_ID` — returns an actionable error if either is unsorted.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn diff(
    old_path: &Path,
    new_path: &Path,
    output: &mut impl Write,
    options: &DiffOptions,
    direct_io: bool,
) -> Result<DiffStats> {
    let _ = (options.ignore_changeset, options.ignore_uid, options.ignore_user);
    let filter = match options.type_filter.as_deref() {
        Some(s) => TypeFilter::parse(s),
        None => TypeFilter::all(),
    };

    // Check sorted headers before opening sequential readers.
    {
        let old_reader = ElementReader::open(old_path, direct_io)?;
        let new_reader = ElementReader::open(new_path, direct_io)?;
        require_sorted(old_reader.header(), old_path, "Old PBF")?;
        require_sorted(new_reader.header(), new_path, "New PBF")?;
    }

    // Check if both files have indexdata for the optimized block-pair path.
    let both_indexed =
        has_indexdata(old_path, direct_io)? && has_indexdata(new_path, direct_io)?;

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

    let phases: [(ElemKind, bool); 3] = [
        (ElemKind::Node, filter.nodes),
        (ElemKind::Way, filter.ways),
        (ElemKind::Relation, filter.relations),
    ];

    for (kind, enabled) in phases {
        if !enabled {
            continue;
        }

        block_pair_merge_phase(
            &mut merge,
            kind,
            &mut |action| {
                match action {
                    BlockMergeAction::BlobEqual(count) => {
                        // v2 doesn't emit BlobEqual yet (reserved for v1 byte comparison).
                        // When it does, we'd need to iterate for per-element output if
                        // !suppress_common. For now, just count.
                        stats.common += count;
                    }
                    BlockMergeAction::BlobOldOnly {
                        block, count, skip,
                    } => {
                        let type_char = super::stream_merge::kind_type_char(kind);
                        for elem in block.elements().skip(skip) {
                            let id = super::stream_merge::element_id(&elem);
                            let ver = super::stream_merge::element_version(&elem);
                            write_compact_line(output, '-', type_char, id, ver)?;
                        }
                        stats.deleted += count;
                    }
                    BlockMergeAction::BlobNewOnly {
                        block, count, skip,
                    } => {
                        let type_char = super::stream_merge::kind_type_char(kind);
                        for elem in block.elements().skip(skip) {
                            let id = super::stream_merge::element_id(&elem);
                            let ver = super::stream_merge::element_version(&elem);
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
                        let type_char = super::stream_merge::kind_type_char(kind);
                        let id = super::stream_merge::element_id(old);
                        let old_ver = super::stream_merge::element_version(old);
                        let new_ver = super::stream_merge::element_version(new);
                        write_modified_line(output, type_char, id, old_ver, new_ver)?;
                        if options.verbose {
                            write_modified_details_borrowed(output, old, new)?;
                        }
                        stats.modified += 1;
                    }
                    BlockMergeAction::ElementOldOnly(o) => {
                        let type_char = super::stream_merge::kind_type_char(kind);
                        let id = super::stream_merge::element_id(o);
                        let ver = super::stream_merge::element_version(o);
                        write_compact_line(output, '-', type_char, id, ver)?;
                        stats.deleted += 1;
                    }
                    BlockMergeAction::ElementNewOnly(n) => {
                        let type_char = super::stream_merge::kind_type_char(kind);
                        let id = super::stream_merge::element_id(n);
                        let ver = super::stream_merge::element_version(n);
                        write_compact_line(output, '+', type_char, id, ver)?;
                        stats.created += 1;
                    }
                }
                Ok(())
            },
        )?;
    }

    Ok(stats)
}

/// Fallback diff path using element-level merge-join with owned elements.
/// Used when either input lacks indexdata.
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
    } else {
        drain_phase::<OwnedNode>(&mut old_src, &mut new_src)?;
    }

    if filter.ways {
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
    } else {
        drain_phase::<OwnedWay>(&mut old_src, &mut new_src)?;
    }

    if filter.relations {
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
    let old_map: HashMap<&str, &str> = old_tags
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let new_map: HashMap<&str, &str> = new_tags
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    write_removed_tags(output, old_tags, &new_map)?;
    write_added_tags(output, new_tags, &old_map)?;
    write_changed_tags(output, new_tags, &old_map)?;
    Ok(())
}

fn write_removed_tags(
    output: &mut dyn Write,
    old_tags: &[(String, String)],
    new_map: &HashMap<&str, &str>,
) -> Result<()> {
    for (k, v) in old_tags {
        if !new_map.contains_key(k.as_str()) {
            writeln!(output, "  -{k}={v}")?;
        }
    }
    Ok(())
}

fn write_added_tags(
    output: &mut dyn Write,
    new_tags: &[(String, String)],
    old_map: &HashMap<&str, &str>,
) -> Result<()> {
    for (k, v) in new_tags {
        if !old_map.contains_key(k.as_str()) {
            writeln!(output, "  +{k}={v}")?;
        }
    }
    Ok(())
}

fn write_changed_tags(
    output: &mut dyn Write,
    new_tags: &[(String, String)],
    old_map: &HashMap<&str, &str>,
) -> Result<()> {
    for (k, new_v) in new_tags {
        if let Some(old_v) = old_map.get(k.as_str())
            && *old_v != new_v.as_str()
        {
            writeln!(output, "  ~{k}: {old_v} -> {new_v}")?;
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
