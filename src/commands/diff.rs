//! Compare two PBF files and output human-readable differences. Equivalent to `osmium diff`.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use super::owned_elements::{
    format_coord, from_decimicro, nodes_equal, read_elements, relations_equal, ways_equal,
    OwnedMember, OwnedNode, OwnedRelation, OwnedWay,
};
use crate::MemberType;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
// Type filter
// ---------------------------------------------------------------------------

struct TypeFilter {
    nodes: bool,
    ways: bool,
    relations: bool,
}

impl TypeFilter {
    fn all() -> Self {
        Self {
            nodes: true,
            ways: true,
            relations: true,
        }
    }
}

fn parse_type_filter(s: &str) -> TypeFilter {
    TypeFilter {
        nodes: s.contains("node"),
        ways: s.contains("way"),
        relations: s.contains("relation"),
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compare two sorted PBF files and write human-readable differences.
#[hotpath::measure]
pub fn diff(
    old_path: &Path,
    new_path: &Path,
    output: &mut impl Write,
    options: &DiffOptions,
    direct_io: bool,
) -> Result<DiffStats> {
    let mut old = read_elements(old_path, direct_io)?;
    let mut new = read_elements(new_path, direct_io)?;

    // Ensure sorted by ID
    old.nodes.sort_by_key(|n| n.id);
    old.ways.sort_by_key(|w| w.id);
    old.relations.sort_by_key(|r| r.id);
    new.nodes.sort_by_key(|n| n.id);
    new.ways.sort_by_key(|w| w.id);
    new.relations.sort_by_key(|r| r.id);

    let filter = match options.type_filter.as_deref() {
        Some(s) => parse_type_filter(s),
        None => TypeFilter::all(),
    };

    let mut stats = DiffStats {
        common: 0,
        created: 0,
        modified: 0,
        deleted: 0,
    };

    if filter.nodes {
        diff_nodes(&old.nodes, &new.nodes, output, options, &mut stats)?;
    }
    if filter.ways {
        diff_ways(&old.ways, &new.ways, output, options, &mut stats)?;
    }
    if filter.relations {
        diff_relations(&old.relations, &new.relations, output, options, &mut stats)?;
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Merge-join per element type
// ---------------------------------------------------------------------------

fn diff_nodes(
    old: &[OwnedNode],
    new: &[OwnedNode],
    output: &mut impl Write,
    opts: &DiffOptions,
    stats: &mut DiffStats,
) -> Result<()> {
    let mut oi = 0;
    let mut ni = 0;

    while oi < old.len() && ni < new.len() {
        match old[oi].id.cmp(&new[ni].id) {
            std::cmp::Ordering::Less => {
                write_compact_line(output, '-', 'n', old[oi].id, old[oi].version)?;
                stats.deleted += 1;
                oi += 1;
            }
            std::cmp::Ordering::Greater => {
                write_compact_line(output, '+', 'n', new[ni].id, new[ni].version)?;
                stats.created += 1;
                ni += 1;
            }
            std::cmp::Ordering::Equal => {
                diff_node_pair(&old[oi], &new[ni], output, opts, stats)?;
                oi += 1;
                ni += 1;
            }
        }
    }

    for o in &old[oi..] {
        write_compact_line(output, '-', 'n', o.id, o.version)?;
        stats.deleted += 1;
    }
    for n in &new[ni..] {
        write_compact_line(output, '+', 'n', n.id, n.version)?;
        stats.created += 1;
    }
    Ok(())
}

fn diff_node_pair(
    old: &OwnedNode,
    new: &OwnedNode,
    output: &mut impl Write,
    opts: &DiffOptions,
    stats: &mut DiffStats,
) -> Result<()> {
    if nodes_equal(old, new) {
        if !opts.suppress_common {
            write_compact_line(output, ' ', 'n', old.id, old.version)?;
        }
        stats.common += 1;
    } else {
        write_modified_line(output, 'n', old.id, old.version, new.version)?;
        if opts.verbose {
            write_node_details(output, old, new)?;
        }
        stats.modified += 1;
    }
    Ok(())
}

fn diff_ways(
    old: &[OwnedWay],
    new: &[OwnedWay],
    output: &mut impl Write,
    opts: &DiffOptions,
    stats: &mut DiffStats,
) -> Result<()> {
    let mut oi = 0;
    let mut ni = 0;

    while oi < old.len() && ni < new.len() {
        match old[oi].id.cmp(&new[ni].id) {
            std::cmp::Ordering::Less => {
                write_compact_line(output, '-', 'w', old[oi].id, old[oi].version)?;
                stats.deleted += 1;
                oi += 1;
            }
            std::cmp::Ordering::Greater => {
                write_compact_line(output, '+', 'w', new[ni].id, new[ni].version)?;
                stats.created += 1;
                ni += 1;
            }
            std::cmp::Ordering::Equal => {
                diff_way_pair(&old[oi], &new[ni], output, opts, stats)?;
                oi += 1;
                ni += 1;
            }
        }
    }

    for o in &old[oi..] {
        write_compact_line(output, '-', 'w', o.id, o.version)?;
        stats.deleted += 1;
    }
    for n in &new[ni..] {
        write_compact_line(output, '+', 'w', n.id, n.version)?;
        stats.created += 1;
    }
    Ok(())
}

fn diff_way_pair(
    old: &OwnedWay,
    new: &OwnedWay,
    output: &mut impl Write,
    opts: &DiffOptions,
    stats: &mut DiffStats,
) -> Result<()> {
    if ways_equal(old, new) {
        if !opts.suppress_common {
            write_compact_line(output, ' ', 'w', old.id, old.version)?;
        }
        stats.common += 1;
    } else {
        write_modified_line(output, 'w', old.id, old.version, new.version)?;
        if opts.verbose {
            write_way_details(output, old, new)?;
        }
        stats.modified += 1;
    }
    Ok(())
}

fn diff_relations(
    old: &[OwnedRelation],
    new: &[OwnedRelation],
    output: &mut impl Write,
    opts: &DiffOptions,
    stats: &mut DiffStats,
) -> Result<()> {
    let mut oi = 0;
    let mut ni = 0;

    while oi < old.len() && ni < new.len() {
        match old[oi].id.cmp(&new[ni].id) {
            std::cmp::Ordering::Less => {
                write_compact_line(output, '-', 'r', old[oi].id, old[oi].version)?;
                stats.deleted += 1;
                oi += 1;
            }
            std::cmp::Ordering::Greater => {
                write_compact_line(output, '+', 'r', new[ni].id, new[ni].version)?;
                stats.created += 1;
                ni += 1;
            }
            std::cmp::Ordering::Equal => {
                diff_relation_pair(&old[oi], &new[ni], output, opts, stats)?;
                oi += 1;
                ni += 1;
            }
        }
    }

    for o in &old[oi..] {
        write_compact_line(output, '-', 'r', o.id, o.version)?;
        stats.deleted += 1;
    }
    for n in &new[ni..] {
        write_compact_line(output, '+', 'r', n.id, n.version)?;
        stats.created += 1;
    }
    Ok(())
}

fn diff_relation_pair(
    old: &OwnedRelation,
    new: &OwnedRelation,
    output: &mut impl Write,
    opts: &DiffOptions,
    stats: &mut DiffStats,
) -> Result<()> {
    if relations_equal(old, new) {
        if !opts.suppress_common {
            write_compact_line(output, ' ', 'r', old.id, old.version)?;
        }
        stats.common += 1;
    } else {
        write_modified_line(output, 'r', old.id, old.version, new.version)?;
        if opts.verbose {
            write_relation_details(output, old, new)?;
        }
        stats.modified += 1;
    }
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
    output: &mut impl Write,
    old: &OwnedNode,
    new: &OwnedNode,
) -> Result<()> {
    if old.decimicro_lat != new.decimicro_lat || old.decimicro_lon != new.decimicro_lon {
        writeln!(
            output,
            "  coordinates: ({}, {}) -> ({}, {})",
            format_coord(from_decimicro(old.decimicro_lat)),
            format_coord(from_decimicro(old.decimicro_lon)),
            format_coord(from_decimicro(new.decimicro_lat)),
            format_coord(from_decimicro(new.decimicro_lon)),
        )?;
    }
    write_tag_diff(output, &old.tags, &new.tags)?;
    Ok(())
}

fn write_way_details(
    output: &mut impl Write,
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
    output: &mut impl Write,
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
    output: &mut impl Write,
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
    output: &mut impl Write,
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
    output: &mut impl Write,
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
    output: &mut impl Write,
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

fn format_member(m: &OwnedMember) -> String {
    format!(
        "{}/{} \"{}\"",
        member_type_str(m.id.member_type()),
        m.id.id(),
        m.role,
    )
}

fn member_matches(a: &OwnedMember, b: &OwnedMember) -> bool {
    a.id == b.id && a.role == b.role
}

fn write_member_diff(
    output: &mut impl Write,
    old_members: &[OwnedMember],
    new_members: &[OwnedMember],
) -> Result<()> {
    // Removed members (in old but not in new)
    for old_m in old_members {
        if !new_members.iter().any(|nm| member_matches(old_m, nm)) {
            writeln!(output, "  -member {}", format_member(old_m))?;
        }
    }
    // Added members (in new but not in old)
    for new_m in new_members {
        if !old_members.iter().any(|om| member_matches(om, new_m)) {
            writeln!(output, "  +member {}", format_member(new_m))?;
        }
    }
    Ok(())
}
