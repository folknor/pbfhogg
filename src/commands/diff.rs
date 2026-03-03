//! Compare two PBF files and output human-readable differences. Equivalent to `osmium diff`.
//!
//! Streams through both files in constant memory using [`StreamingBlocks`] cursors.
//! Requires both inputs to declare `Sort.Type_then_ID`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use super::owned_elements::{
    format_coord, from_decimicro, nodes_equal, relations_equal, ways_equal, OwnedMember,
    OwnedNode, OwnedRelation, OwnedWay,
};
use super::stream_merge::{
    convert_node, convert_relation, convert_way, is_node_block, is_relation_block, is_way_block,
    next_element, StreamingBlocks,
};
use super::{require_sorted, Result, TypeFilter};
use crate::{BlobFilter, BlockType, Element, ElementReader, MemberType};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Options for the diff command.
pub struct DiffOptions {
    /// Hide unchanged elements (show only created/modified/deleted).
    pub suppress_common: bool,
    /// Show detailed changes for modified elements.
    pub verbose: bool,
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

    /// Print summary to stderr.
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
}

// ---------------------------------------------------------------------------
// DiffElement trait — per-type accessors for the generic merge-join
// ---------------------------------------------------------------------------

trait DiffElement: Sized {
    fn id(&self) -> i64;
    fn version(&self) -> Option<i32>;
    fn type_char() -> char;
    fn is_block_type(bt: BlockType) -> bool;
    fn equal(&self, other: &Self) -> bool;
    fn convert(element: &Element<'_>) -> Option<Self>;
}

impl DiffElement for OwnedNode {
    fn id(&self) -> i64 { self.id }
    fn version(&self) -> Option<i32> { self.version }
    fn type_char() -> char { 'n' }
    fn is_block_type(bt: BlockType) -> bool { is_node_block(bt) }
    fn equal(&self, other: &Self) -> bool { nodes_equal(self, other) }
    fn convert(element: &Element<'_>) -> Option<Self> { convert_node(element) }
}

impl DiffElement for OwnedWay {
    fn id(&self) -> i64 { self.id }
    fn version(&self) -> Option<i32> { self.version }
    fn type_char() -> char { 'w' }
    fn is_block_type(bt: BlockType) -> bool { is_way_block(bt) }
    fn equal(&self, other: &Self) -> bool { ways_equal(self, other) }
    fn convert(element: &Element<'_>) -> Option<Self> { convert_way(element) }
}

impl DiffElement for OwnedRelation {
    fn id(&self) -> i64 { self.id }
    fn version(&self) -> Option<i32> { self.version }
    fn type_char() -> char { 'r' }
    fn is_block_type(bt: BlockType) -> bool { is_relation_block(bt) }
    fn equal(&self, other: &Self) -> bool { relations_equal(self, other) }
    fn convert(element: &Element<'_>) -> Option<Self> { convert_relation(element) }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compare two sorted PBF files and write human-readable differences.
///
/// Streams through both files in constant memory (~3 MB overhead per cursor)
/// using pipelined block iterators. Requires both inputs to declare
/// `Sort.Type_then_ID` — returns an actionable error if either is unsorted.
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

    // Open readers and check sorted headers before applying any filters.
    let old_reader = ElementReader::open(old_path, direct_io)?;
    let new_reader = ElementReader::open(new_path, direct_io)?;

    require_sorted(old_reader.header(), old_path, "Old PBF")?;
    require_sorted(new_reader.header(), new_path, "New PBF")?;

    // Apply blob filter for type-filtered queries (skips decompressing
    // irrelevant blob types when indexdata is present).
    let blob_filter = if filter.nodes && filter.ways && filter.relations {
        None
    } else {
        Some(BlobFilter::new(filter.nodes, filter.ways, filter.relations))
    };
    let old_reader = match blob_filter.clone() {
        Some(f) => old_reader.with_blob_filter(f),
        None => old_reader,
    };
    let new_reader = match blob_filter {
        Some(f) => new_reader.with_blob_filter(f),
        None => new_reader,
    };

    // Build streaming cursors — two concurrent pipelined decoders.
    let mut old_src = StreamingBlocks::new(old_reader.into_blocks_pipelined());
    let mut new_src = StreamingBlocks::new(new_reader.into_blocks_pipelined());

    let mut stats = DiffStats { common: 0, created: 0, modified: 0, deleted: 0 };

    // Phase 1: Nodes
    // Each phase uses local buffers — T changes between phases so they cannot
    // be shared. Allocation is negligible (one block's worth, up to 8000 elements).
    if filter.nodes {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        streaming_diff_phase::<OwnedNode>(
            &mut old_src, &mut ob, &mut new_src, &mut nb,
            (output, options, &mut stats),
            |out, old, new| write_node_details(out, old, new),
        )?;
    } else {
        drain_phase::<OwnedNode>(&mut old_src)?;
        drain_phase::<OwnedNode>(&mut new_src)?;
    }

    // Phase 2: Ways
    if filter.ways {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        streaming_diff_phase::<OwnedWay>(
            &mut old_src, &mut ob, &mut new_src, &mut nb,
            (output, options, &mut stats),
            |out, old, new| write_way_details(out, old, new),
        )?;
    } else {
        drain_phase::<OwnedWay>(&mut old_src)?;
        drain_phase::<OwnedWay>(&mut new_src)?;
    }

    // Phase 3: Relations
    if filter.relations {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        streaming_diff_phase::<OwnedRelation>(
            &mut old_src, &mut ob, &mut new_src, &mut nb,
            (output, options, &mut stats),
            |out, old, new| write_relation_details(out, old, new),
        )?;
    } else {
        drain_phase::<OwnedRelation>(&mut old_src)?;
        drain_phase::<OwnedRelation>(&mut new_src)?;
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Generic streaming merge-join
// ---------------------------------------------------------------------------

/// Streaming two-pointer merge-join for one element type phase.
///
/// Pulls elements one at a time from both cursors and emits diff output
/// immediately. Stops when both cursors return `None` (phase exhausted).
fn streaming_diff_phase<T: DiffElement>(
    old_src: &mut StreamingBlocks,
    old_buf: &mut Vec<T>,
    new_src: &mut StreamingBlocks,
    new_buf: &mut Vec<T>,
    ctx: (&mut impl Write, &DiffOptions, &mut DiffStats),
    write_details: impl Fn(&mut dyn Write, &T, &T) -> Result<()>,
) -> Result<()> {
    let (output, opts, stats) = ctx;
    let mut old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
    let mut new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;

    loop {
        match (&old_elem, &new_elem) {
            (None, None) => break,
            (Some(o), None) => {
                write_compact_line(output, '-', T::type_char(), o.id(), o.version())?;
                stats.deleted += 1;
                old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
            }
            (None, Some(n)) => {
                write_compact_line(output, '+', T::type_char(), n.id(), n.version())?;
                stats.created += 1;
                new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;
            }
            (Some(o), Some(n)) => {
                emit_matched_pair(o, n, output, opts, stats, &write_details)?;
                match o.id().cmp(&n.id()) {
                    Ordering::Less => {
                        old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
                    }
                    Ordering::Greater => {
                        new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;
                    }
                    Ordering::Equal => {
                        old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
                        new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Emit output for a pair where both old and new are present.
///
/// Factored out to keep `streaming_diff_phase` under clippy's cognitive
/// complexity threshold.
fn emit_matched_pair<T: DiffElement>(
    old: &T,
    new: &T,
    output: &mut impl Write,
    opts: &DiffOptions,
    stats: &mut DiffStats,
    write_details: &impl Fn(&mut dyn Write, &T, &T) -> Result<()>,
) -> Result<()> {
    match old.id().cmp(&new.id()) {
        Ordering::Less => {
            write_compact_line(output, '-', T::type_char(), old.id(), old.version())?;
            stats.deleted += 1;
        }
        Ordering::Greater => {
            write_compact_line(output, '+', T::type_char(), new.id(), new.version())?;
            stats.created += 1;
        }
        Ordering::Equal => {
            if T::equal(old, new) {
                if !opts.suppress_common {
                    write_compact_line(output, ' ', T::type_char(), old.id(), old.version())?;
                }
                stats.common += 1;
            } else {
                write_modified_line(output, T::type_char(), old.id(), old.version(), new.version())?;
                if opts.verbose {
                    write_details(output, old, new)?;
                }
                stats.modified += 1;
            }
        }
    }
    Ok(())
}

/// Drain remaining elements of type `T` from a cursor without processing.
///
/// Called to advance past a skipped phase (e.g. when type_filter excludes
/// nodes) so the cursor is positioned for the next phase.
fn drain_phase<T: DiffElement>(source: &mut StreamingBlocks) -> Result<()> {
    let mut buf = Vec::new();
    while next_element(source, &mut buf, T::is_block_type, T::convert)?.is_some() {}
    Ok(())
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
