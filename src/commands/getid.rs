//! Extract or remove elements by ID. Equivalent to `osmium getid` / `osmium removeid`.

use std::collections::BTreeSet;
use std::path::Path;

use super::{dense_node_metadata, element_metadata, flush_block, rebuild_header};
use crate::block_builder::{BlockBuilder, MemberData};
use crate::file_writer::FileWriter;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobDecode, BlobReader, Element};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// ID parsing
// ---------------------------------------------------------------------------

/// Parsed element IDs grouped by type.
pub struct IdSet {
    pub node_ids: BTreeSet<i64>,
    pub way_ids: BTreeSet<i64>,
    pub relation_ids: BTreeSet<i64>,
}


/// Parse an ID spec like "n123", "w456", "r789".
// String errors are intentional — shows the bad input value, which is more helpful
// for CLI users than the underlying ParseIntError.
fn parse_id_spec(spec: &str) -> Result<(char, i64)> {
    if spec.len() < 2 {
        return Err(format!("invalid ID spec: {spec:?} (expected n/w/r prefix + number)").into());
    }
    let prefix = spec.as_bytes()[0];
    if !matches!(prefix, b'n' | b'w' | b'r') {
        return Err(
            format!("invalid ID spec: {spec:?} (expected prefix 'n', 'w', or 'r')").into(),
        );
    }
    let id: i64 = spec[1..]
        .parse()
        .map_err(|_| format!("invalid ID spec: {spec:?} (bad number)"))?;
    Ok((prefix as char, id))
}

/// Parse ID specs from command-line arguments.
pub fn parse_ids(specs: &[String]) -> Result<IdSet> {
    let mut set = IdSet {
        node_ids: BTreeSet::new(),
        way_ids: BTreeSet::new(),
        relation_ids: BTreeSet::new(),
    };
    for spec in specs {
        let (prefix, id) = parse_id_spec(spec)?;
        match prefix {
            'n' => set.node_ids.insert(id),
            'w' => set.way_ids.insert(id),
            'r' => set.relation_ids.insert(id),
            _ => unreachable!(),
        };
    }
    Ok(set)
}

/// Parse ID specs from a file (one per line, blank lines and `#` comments skipped).
pub fn parse_ids_from_file(path: &Path) -> Result<IdSet> {
    let contents = std::fs::read_to_string(path)?;
    let specs: Vec<String> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .collect();
    parse_ids(&specs)
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from a getid/removeid operation.
pub struct GetidStats {
    pub nodes_written: u64,
    pub ways_written: u64,
    pub relations_written: u64,
}

impl GetidStats {
    pub fn print_summary(&self) {
        let total = self.nodes_written + self.ways_written + self.relations_written;
        eprintln!(
            "Wrote {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        );
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract elements matching the given IDs.
///
/// If `add_referenced` is true, referenced nodes of matching ways are also
/// included (two-pass). Otherwise, only exact ID matches are output.
#[hotpath::measure]
pub fn getid(
    input: &Path,
    output: &Path,
    ids: &IdSet,
    add_referenced: bool,
    compression: Compression,
    direct_io: bool,
) -> Result<GetidStats> {
    if add_referenced {
        getid_with_refs(input, output, ids, compression, direct_io)
    } else {
        filter_by_id(input, output, ids, true, compression, direct_io)
    }
}

/// Remove elements matching the given IDs (output everything else).
#[hotpath::measure]
pub fn removeid(input: &Path, output: &Path, ids: &IdSet, compression: Compression, direct_io: bool) -> Result<GetidStats> {
    filter_by_id(input, output, ids, false, compression, direct_io)
}

// ---------------------------------------------------------------------------
// Single-pass filter (shared by getid without refs and removeid)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn filter_by_id(
    input: &Path,
    output: &Path,
    ids: &IdSet,
    include: bool,
    compression: Compression,
    direct_io: bool,
) -> Result<GetidStats> {
    let mut writer = PbfWriter::to_path(output, compression)?;
    let mut bb = BlockBuilder::new();
    let mut header_written = false;
    let mut stats = GetidStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    // Fast path: if the ID set is empty, include mode writes nothing,
    // exclude mode writes everything (passthrough would be better but
    // we still need to rebuild the header).
    let reader = BlobReader::open(input, direct_io)?;

    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                if !header_written {
                    rebuild_header(&header, &mut writer, header.is_sorted())?;
                    header_written = true;
                }
            }
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    let dominated = match &element {
                        Element::DenseNode(dn) => ids.node_ids.contains(&dn.id()),
                        Element::Node(n) => ids.node_ids.contains(&n.id()),
                        Element::Way(w) => ids.way_ids.contains(&w.id()),
                        Element::Relation(r) => ids.relation_ids.contains(&r.id()),
                    };
                    let write = if include { dominated } else { !dominated };
                    if write {
                        write_element(&element, &mut bb, &mut writer, &mut stats)?;
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Two-pass getid with --add-referenced
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn getid_with_refs(input: &Path, output: &Path, ids: &IdSet, compression: Compression, direct_io: bool) -> Result<GetidStats> {
    let mut stats = GetidStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    // Pass 1: Collect ref node IDs from matching ways.
    let mut dep_node_ids: BTreeSet<i64> = BTreeSet::new();

    if !ids.way_ids.is_empty() {
        let reader = BlobReader::open(input, direct_io)?;
        for blob in reader {
            let blob = blob?;
            if let BlobDecode::OsmData(block) = blob.decode()? {
                for element in block.elements() {
                    if let Element::Way(w) = &element
                        && ids.way_ids.contains(&w.id())
                    {
                        dep_node_ids.extend(w.refs());
                    }
                }
            }
        }
    }

    // Pass 2: Write matching elements + dependent nodes.
    let mut writer = PbfWriter::to_path(output, compression)?;
    let mut bb = BlockBuilder::new();
    let mut header_written = false;

    let reader = BlobReader::open(input, direct_io)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                if !header_written {
                    rebuild_header(&header, &mut writer, header.is_sorted())?;
                    header_written = true;
                }
            }
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    let write = match &element {
                        Element::DenseNode(dn) => {
                            ids.node_ids.contains(&dn.id())
                                || dep_node_ids.contains(&dn.id())
                        }
                        Element::Node(n) => {
                            ids.node_ids.contains(&n.id())
                                || dep_node_ids.contains(&n.id())
                        }
                        Element::Way(w) => ids.way_ids.contains(&w.id()),
                        Element::Relation(r) => ids.relation_ids.contains(&r.id()),
                    };
                    if write {
                        write_element(&element, &mut bb, &mut writer, &mut stats)?;
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Helpers
// wontfix(perf-drain-reuse): tags/refs/members Vec collected fresh per element.
// Hoisting buffers per cat.rs pattern would avoid allocations at planet scale.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn write_element(
    element: &Element<'_>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut GetidStats,
) -> Result<()> {
    match element {
        Element::DenseNode(dn) => {
            if !bb.can_add_node() {
                flush_block(bb, writer)?;
            }
            let tags: Vec<(&str, &str)> = dn.tags().collect();
            let meta = dense_node_metadata(dn);
            bb.add_node(
                dn.id(),
                dn.decimicro_lat(),
                dn.decimicro_lon(),
                &tags,
                meta.as_ref(),
            );
            stats.nodes_written += 1;
        }
        Element::Node(n) => {
            if !bb.can_add_node() {
                flush_block(bb, writer)?;
            }
            let tags: Vec<(&str, &str)> = n.tags().collect();
            let meta = element_metadata(&n.info());
            bb.add_node(
                n.id(),
                n.decimicro_lat(),
                n.decimicro_lon(),
                &tags,
                meta.as_ref(),
            );
            stats.nodes_written += 1;
        }
        Element::Way(w) => {
            if !bb.can_add_way() {
                flush_block(bb, writer)?;
            }
            let tags: Vec<(&str, &str)> = w.tags().collect();
            let refs: Vec<i64> = w.refs().collect();
            let meta = element_metadata(&w.info());
            bb.add_way(w.id(), &tags, &refs, meta.as_ref());
            stats.ways_written += 1;
        }
        Element::Relation(r) => {
            if !bb.can_add_relation() {
                flush_block(bb, writer)?;
            }
            let tags: Vec<(&str, &str)> = r.tags().collect();
            let members: Vec<MemberData<'_>> = r
                .members()
                .map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                })
                .collect();
            let meta = element_metadata(&r.info());
            bb.add_relation(r.id(), &tags, &members, meta.as_ref());
            stats.relations_written += 1;
        }
    }
    Ok(())
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Tests use `unwrap()` throughout because panicking is the correct failure mode
// for unit tests -- it immediately fails the test with a clear backtrace pointing
// to the exact call site. Propagating Results via `-> Result<()>` in tests would
// lose the backtrace and produce less actionable error messages. The crate-wide
// `unwrap_used = "deny"` lint is designed for production code where panics are
// unacceptable; test code is exempt via this module-level allow.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_node_id() {
        let (prefix, id) = parse_id_spec("n123").unwrap();
        assert_eq!(prefix, 'n');
        assert_eq!(id, 123);
    }

    #[test]
    fn parse_way_id() {
        let (prefix, id) = parse_id_spec("w456").unwrap();
        assert_eq!(prefix, 'w');
        assert_eq!(id, 456);
    }

    #[test]
    fn parse_relation_id() {
        let (prefix, id) = parse_id_spec("r789").unwrap();
        assert_eq!(prefix, 'r');
        assert_eq!(id, 789);
    }

    #[test]
    fn parse_large_id() {
        let (prefix, id) = parse_id_spec("n9876543210").unwrap();
        assert_eq!(prefix, 'n');
        assert_eq!(id, 9_876_543_210);
    }

    #[test]
    fn parse_invalid_prefix() {
        assert!(parse_id_spec("x123").is_err());
    }

    #[test]
    fn parse_missing_number() {
        assert!(parse_id_spec("n").is_err());
    }

    #[test]
    fn parse_bad_number() {
        assert!(parse_id_spec("nabc").is_err());
    }

    #[test]
    fn parse_too_short() {
        assert!(parse_id_spec("n").is_err());
        assert!(parse_id_spec("").is_err());
    }

    #[test]
    fn parse_ids_mixed() {
        let specs: Vec<String> = vec!["n1", "n2", "w10", "r100"]
            .into_iter()
            .map(ToString::to_string)
            .collect();
        let set = parse_ids(&specs).unwrap();
        assert_eq!(set.node_ids, BTreeSet::from([1, 2]));
        assert_eq!(set.way_ids, BTreeSet::from([10]));
        assert_eq!(set.relation_ids, BTreeSet::from([100]));
    }

}
