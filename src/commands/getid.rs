//! Extract or remove elements by ID. Equivalent to `osmium getid` / `osmium removeid`.

use std::collections::BTreeSet;
use std::path::Path;

use rayon::prelude::*;

use super::{dense_node_metadata, element_metadata, flush_local, require_indexdata};
use crate::block_builder::{HeaderBuilder, BlockBuilder, MemberData, OwnedBlock};
use crate::file_writer::FileWriter;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, PrimitiveBlock};

use super::{Result, BATCH_SIZE};

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
    force: bool,
) -> Result<GetidStats> {
    require_indexdata(input, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, the type filter \
         based on requested ID types is a no-op — all blobs are decompressed \
         (significantly slower).")?;

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

fn filter_by_id(
    input: &Path,
    output: &Path,
    ids: &IdSet,
    include: bool,
    compression: Compression,
    direct_io: bool,
) -> Result<GetidStats> {
    let reader = ElementReader::open(input, direct_io)?;
    // Skip blob types with no matching IDs (getid only — removeid needs all types).
    let reader = if include {
        reader.with_blob_filter(BlobFilter::new(
            !ids.node_ids.is_empty(),
            !ids.way_ids.is_empty(),
            !ids.relation_ids.is_empty(),
        ))
    } else {
        reader
    };
    let mut hb = HeaderBuilder::from_header(reader.header());
    if reader.header().is_sorted() {
        hb = hb.sorted();
    }
    let header_bytes = hb.build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;
    let mut stats = GetidStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    for block in reader.into_blocks_pipelined() {
        let block = block?;
        batch.push(block);

        if batch.len() >= BATCH_SIZE {
            let (nodes, ways, relations) = process_filter_batch(
                &batch, &mut writer, ids, include, None,
            )?;
            stats.nodes_written += nodes;
            stats.ways_written += ways;
            stats.relations_written += relations;
            batch.clear();
        }
    }

    if !batch.is_empty() {
        let (nodes, ways, relations) = process_filter_batch(
            &batch, &mut writer, ids, include, None,
        )?;
        stats.nodes_written += nodes;
        stats.ways_written += ways;
        stats.relations_written += relations;
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Two-pass getid with --add-referenced
// ---------------------------------------------------------------------------

fn getid_with_refs(input: &Path, output: &Path, ids: &IdSet, compression: Compression, direct_io: bool) -> Result<GetidStats> {
    let mut stats = GetidStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    // Pass 1: Collect ref node IDs from matching ways (sequential — collecting
    // into a BTreeSet which would need merge, and the way_ids set is usually small).
    let mut dep_node_ids: BTreeSet<i64> = BTreeSet::new();

    if !ids.way_ids.is_empty() {
        // Pass 1: only scan way blobs to collect referenced node IDs.
        let reader = ElementReader::open(input, direct_io)?
            .with_blob_filter(BlobFilter::only_ways());
        for block in reader.into_blocks_pipelined() {
            let block = block?;
            for element in block.elements() {
                if let Element::Way(w) = &element
                    && ids.way_ids.contains(&w.id())
                {
                    dep_node_ids.extend(w.refs());
                }
            }
        }
    }

    // Pass 2: Write matching elements + dependent nodes (parallel batches).
    let reader = ElementReader::open(input, direct_io)?;
    // Skip blob types not needed: nodes if no node IDs and no dependent nodes,
    // ways always needed (add-referenced mode), relations if no relation IDs.
    let reader = reader.with_blob_filter(BlobFilter::new(
        !ids.node_ids.is_empty() || !dep_node_ids.is_empty(),
        true,
        !ids.relation_ids.is_empty(),
    ));
    let mut hb = HeaderBuilder::from_header(reader.header());
    if reader.header().is_sorted() {
        hb = hb.sorted();
    }
    let header_bytes = hb.build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;

    let dep_ref = if dep_node_ids.is_empty() { None } else { Some(&dep_node_ids) };

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    for block in reader.into_blocks_pipelined() {
        let block = block?;
        batch.push(block);

        if batch.len() >= BATCH_SIZE {
            let (nodes, ways, relations) = process_filter_batch(
                &batch, &mut writer, ids, true, dep_ref,
            )?;
            stats.nodes_written += nodes;
            stats.ways_written += ways;
            stats.relations_written += relations;
            batch.clear();
        }
    }

    if !batch.is_empty() {
        let (nodes, ways, relations) = process_filter_batch(
            &batch, &mut writer, ids, true, dep_ref,
        )?;
        stats.nodes_written += nodes;
        stats.ways_written += ways;
        stats.relations_written += relations;
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Parallel batch processing
// ---------------------------------------------------------------------------

/// Process a single `PrimitiveBlock` through the ID filter, writing matching
/// elements into the thread-local `BlockBuilder` and flushing complete blocks
/// into `output`. Returns `(nodes, ways, relations)` counts.
///
/// Called from rayon worker threads via `map_init`.
fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    ids: &IdSet,
    include: bool,
    dep_node_ids: Option<&BTreeSet<i64>>,
) -> std::result::Result<(u64, u64, u64), String> {
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;

    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        let dominated = match &element {
            Element::DenseNode(dn) => {
                ids.node_ids.contains(&dn.id())
                    || dep_node_ids.is_some_and(|deps| deps.contains(&dn.id()))
            }
            Element::Node(n) => {
                ids.node_ids.contains(&n.id())
                    || dep_node_ids.is_some_and(|deps| deps.contains(&n.id()))
            }
            Element::Way(w) => ids.way_ids.contains(&w.id()),
            Element::Relation(r) => ids.relation_ids.contains(&r.id()),
        };
        let emit = if include { dominated } else { !dominated };
        if !emit {
            continue;
        }

        match &element {
            Element::DenseNode(dn) => {
                if !bb.can_add_node() {
                    flush_local(bb, output)?;
                }
                tags_buf.clear();
                tags_buf.extend(dn.tags());
                let meta = dense_node_metadata(dn);
                bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags_buf, meta.as_ref());
                nodes += 1;
            }
            Element::Node(n) => {
                if !bb.can_add_node() {
                    flush_local(bb, output)?;
                }
                tags_buf.clear();
                tags_buf.extend(n.tags());
                let meta = element_metadata(&n.info());
                bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                nodes += 1;
            }
            Element::Way(w) => {
                if !bb.can_add_way() {
                    flush_local(bb, output)?;
                }
                tags_buf.clear();
                tags_buf.extend(w.tags());
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = element_metadata(&w.info());
                bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                ways += 1;
            }
            Element::Relation(r) => {
                if !bb.can_add_relation() {
                    flush_local(bb, output)?;
                }
                tags_buf.clear();
                tags_buf.extend(r.tags());
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                relations += 1;
            }
        }
    }

    Ok((nodes, ways, relations))
}

/// Process a batch of `PrimitiveBlock`s in parallel via rayon.
///
/// Each rayon worker thread owns a `BlockBuilder` (via `map_init`) and
/// processes one block at a time, flushing serialized output to a local
/// `Vec<OwnedBlock>`. After parallel processing, the serialized
/// blocks are written sequentially to the `PbfWriter` in batch order.
///
/// Returns `(nodes_written, ways_written, relations_written)`.
fn process_filter_batch(
    batch: &[PrimitiveBlock],
    writer: &mut PbfWriter<FileWriter>,
    ids: &IdSet,
    include: bool,
    dep_node_ids: Option<&BTreeSet<i64>>,
) -> Result<(u64, u64, u64)> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, u64, u64, u64), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let (nodes, ways, relations) = process_block(
                    block, bb, &mut output, ids, include, dep_node_ids,
                )?;
                flush_local(bb, &mut output)?;
                Ok((output, nodes, ways, relations))
            },
        )
        .collect();

    let mut total_nodes: u64 = 0;
    let mut total_ways: u64 = 0;
    let mut total_relations: u64 = 0;

    for result in results {
        let (blocks, nodes, ways, relations) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        total_nodes += nodes;
        total_ways += ways;
        total_relations += relations;
        for (block_bytes, index, tagdata) in blocks {
            writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
        }
    }

    Ok((total_nodes, total_ways, total_relations))
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
