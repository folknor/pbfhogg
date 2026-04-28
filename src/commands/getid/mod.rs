//! Extract or remove elements by ID. Equivalent to `osmium getid` / `osmium removeid`.

use std::path::Path;

use crate::idset::IdSet;

use rayon::prelude::*;

use super::{
    drain_batch_results, flush_local, require_indexdata,
    for_each_primitive_block_batch, writer_from_header, ensure_node_capacity_local,
    ensure_way_capacity_local, ensure_relation_capacity_local, HeaderOverrides,
};
use crate::owned::{dense_node_metadata, element_metadata};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_writer::FileWriter;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, PrimitiveBlock};

use super::{Result, BATCH_SIZE};

// ---------------------------------------------------------------------------
// ID parsing
// ---------------------------------------------------------------------------

/// Parsed element IDs grouped by type.
/// Uses `IdSet` for O(1) membership testing at all scales.
pub struct ElementIds {
    pub node_ids: IdSet,
    pub way_ids: IdSet,
    pub relation_ids: IdSet,
}

/// Element type used as default when parsing bare numeric IDs.
#[derive(Clone, Copy)]
pub enum DefaultType {
    Node,
    Way,
    Relation,
}

impl DefaultType {
    fn prefix(self) -> char {
        match self {
            Self::Node => 'n',
            Self::Way => 'w',
            Self::Relation => 'r',
        }
    }
}

/// Parse an ID spec like "n123", "w456", "r789".
// String errors are intentional - shows the bad input value, which is more helpful
// for CLI users than the underlying ParseIntError.
fn parse_id_spec(spec: &str, default_type: Option<DefaultType>) -> Result<(char, i64)> {
    let (prefix, id) = parse_id_spec_inner(spec, default_type)?;
    if id < 0 {
        let kind = match prefix {
            'n' => "node",
            'w' => "way",
            'r' => "relation",
            _ => unreachable!(),
        };
        return Err(format!(
            "getid requires non-negative input ids. \
             Input contains {kind} id {id}. \
             Negative ids are JOSM editor-local staging identifiers \
             that should be resolved before processing."
        )
        .into());
    }
    Ok((prefix, id))
}

fn parse_id_spec_inner(spec: &str, default_type: Option<DefaultType>) -> Result<(char, i64)> {
    if spec.len() < 2 {
        if let Some(default) = default_type {
            let id: i64 = spec
                .parse()
                .map_err(|_| format!("invalid ID spec: {spec:?} (bad number)"))?;
            return Ok((default.prefix(), id));
        }
        return Err(format!("invalid ID spec: {spec:?} (expected n/w/r prefix + number)").into());
    }
    let prefix = spec.as_bytes()[0];
    if !matches!(prefix, b'n' | b'w' | b'r')
        && let Some(default) = default_type
    {
        let id: i64 = spec
            .parse()
            .map_err(|_| format!("invalid ID spec: {spec:?} (bad number)"))?;
        return Ok((default.prefix(), id));
    }
    if !matches!(prefix, b'n' | b'w' | b'r') {
        return Err(format!("invalid ID spec: {spec:?} (expected prefix 'n', 'w', or 'r')").into());
    }
    let id: i64 = spec[1..]
        .parse()
        .map_err(|_| format!("invalid ID spec: {spec:?} (bad number)"))?;
    Ok((prefix as char, id))
}

/// Parse ID specs from command-line arguments.
pub fn parse_ids(specs: &[String]) -> Result<ElementIds> {
    parse_ids_with_default_type(specs, None)
}

/// Parse ID specs from command-line arguments with optional default element type for bare IDs.
pub fn parse_ids_with_default_type(
    specs: &[String],
    default_type: Option<DefaultType>,
) -> Result<ElementIds> {
    let mut set = ElementIds {
        node_ids: IdSet::new(),
        way_ids: IdSet::new(),
        relation_ids: IdSet::new(),
    };
    for spec in specs {
        let (prefix, id) = parse_id_spec(spec, default_type)?;
        match prefix {
            'n' => set.node_ids.set(id),
            'w' => set.way_ids.set(id),
            'r' => set.relation_ids.set(id),
            _ => unreachable!(),
        }
    }
    Ok(set)
}

/// Parse ID specs from a file (one per line, blank lines and `#` comments skipped).
pub fn parse_ids_from_file(path: &Path) -> Result<ElementIds> {
    parse_ids_from_file_with_default_type(path, None)
}

/// Parse ID specs from file with optional default element type for bare IDs.
pub fn parse_ids_from_file_with_default_type(
    path: &Path,
    default_type: Option<DefaultType>,
) -> Result<ElementIds> {
    let contents = std::fs::read_to_string(path)?;
    let specs: Vec<String> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .collect();
    parse_ids_with_default_type(&specs, default_type)
}

/// Collect all element IDs from a PBF file.
///
/// Reads every node, way, and relation in the file and adds its ID to the
/// returned `ElementIds`. No member or reference IDs are collected - only
/// top-level element IDs (matching osmium's `--id-osm-file` behavior).
pub fn parse_ids_from_pbf(path: &Path, _direct_io: bool) -> Result<ElementIds> {
    let mut set = ElementIds {
        node_ids: IdSet::new(),
        way_ids: IdSet::new(),
        relation_ids: IdSet::new(),
    };

    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(path, None)?;

    struct IdBatch {
        node_ids: Vec<i64>,
        way_ids: Vec<i64>,
        relation_ids: Vec<i64>,
    }

    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        None,
        || (),
        |block, _s| {
            let mut batch = IdBatch {
                node_ids: Vec::new(),
                way_ids: Vec::new(),
                relation_ids: Vec::new(),
            };
            for element in block.elements_skip_metadata() {
                match &element {
                    Element::DenseNode(dn) => batch.node_ids.push(dn.id()),
                    Element::Node(n) => batch.node_ids.push(n.id()),
                    Element::Way(w) => batch.way_ids.push(w.id()),
                    Element::Relation(r) => batch.relation_ids.push(r.id()),
                }
            }
            batch
        },
        |_seq, batch| {
            for id in batch.node_ids { set.node_ids.set(id); }
            for id in batch.way_ids { set.way_ids.set(id); }
            for id in batch.relation_ids { set.relation_ids.set(id); }
        },
    )?;

    Ok(set)
}

/// Merge two `ElementIds`s together (union).
pub fn merge_id_sets(a: &mut ElementIds, b: &ElementIds) {
    a.node_ids.merge_from(&b.node_ids);
    a.way_ids.merge_from(&b.way_ids);
    a.relation_ids.merge_from(&b.relation_ids);
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Options for getid.
pub struct GetidOptions {
    /// Include referenced nodes of matching ways (two-pass).
    pub add_referenced: bool,
    /// Strip tags from referenced objects not explicitly requested.
    /// Only meaningful with `add_referenced`.
    pub remove_tags: bool,
}

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
/// If `opts.add_referenced` is true, referenced nodes of matching ways are also
/// included (two-pass). Otherwise, only exact ID matches are output.
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
pub fn getid(
    input: &Path,
    output: &Path,
    ids: &ElementIds,
    opts: &GetidOptions,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<GetidStats> {
    require_indexdata(input, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, the type filter \
         based on requested ID types is a no-op - all blobs are decompressed \
         (significantly slower).")?;

    let result = if opts.add_referenced {
        getid_with_refs(input, output, ids, opts, compression, direct_io, overrides)
    } else {
        filter_by_id(input, output, ids, true, compression, direct_io, overrides)
    }?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("getid_nodes_written", result.nodes_written as i64);
        crate::debug::emit_counter("getid_ways_written", result.ways_written as i64);
        crate::debug::emit_counter("getid_relations_written", result.relations_written as i64);
    }
    Ok(result)
}

/// Remove elements matching the given IDs (output everything else).
///
/// Requires blob-level indexdata so the invert-mode raw-passthrough fast
/// path at `filter_by_id` can skip re-encoding non-matching blobs. On a
/// non-indexed PBF the fast path is unreachable and every blob would
/// decode-and-re-encode silently; `--force` overrides.
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
pub fn removeid(input: &Path, output: &Path, ids: &ElementIds, compression: Compression, direct_io: bool, force: bool, overrides: &HeaderOverrides) -> Result<GetidStats> {
    require_indexdata(input, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, the \
         invert-mode raw-passthrough fast path is unreachable and every \
         blob is decompressed and re-encoded (significantly slower).")?;
    filter_by_id(input, output, ids, false, compression, direct_io, overrides)
}

// ---------------------------------------------------------------------------
// Single-pass filter (shared by getid without refs and removeid).
// Include mode: skip non-matching blobs by type and ID range.
// Invert mode: raw passthrough for non-matching blobs.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn filter_by_id(
    input: &Path,
    output: &Path,
    ids: &ElementIds,
    include: bool,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<GetidStats> {
    use crate::blob::{decode_blob_to_headerblock, BlobKind};
    use crate::read::header_walker::HeaderWalker;

    crate::debug::emit_marker("GETID_SCAN_START");

    // Single pread-based pass over blob headers. Data bodies are only
    // read (via pread) when the blob matters: OsmHeader (once, to emit
    // the output header), OsmData-with-match in include mode, every
    // OsmData in invert mode (the raw-passthrough path still needs the
    // frame bytes). Include mode with a small ID set thus reads only
    // the header index (~140 MB at planet) plus a handful of matching
    // blob bodies.
    let mut walker = HeaderWalker::open(input)?;
    let mut data_buf: Vec<u8> = Vec::new();
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut bb = BlockBuilder::new();
    let mut output_blocks: Vec<crate::block_builder::OwnedBlock> = Vec::new();
    let mut stats = GetidStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };
    let blob_filter = BlobFilter::new(
        ids.node_ids.has_any(),
        ids.way_ids.has_any(),
        ids.relation_ids.has_any(),
    );
    let mut blobs_skipped: u64 = 0;
    let mut blobs_passthrough: u64 = 0;

    // Find and decode the leading OsmHeader blob to build the output header.
    let mut writer: Option<PbfWriter<FileWriter>> = None;

    while let Some(meta) = walker.next_header()? {
        match meta.blob_type {
            BlobKind::OsmHeader => {
                walker.pread_data(meta.data_offset, meta.data_size, &mut data_buf)?;
                let header = decode_blob_to_headerblock(&data_buf)?;
                super::warn_locations_on_ways_loss(&header);
                let header_bytes =
                    super::build_output_header(&header, true, overrides, |hb| hb)?;
                writer = Some(super::writer_from_header_bytes(
                    output, compression, &header_bytes, direct_io, false,
                )?);
            }
            BlobKind::OsmData => {
                let w = writer.as_mut().ok_or("no OSMHeader blob found before OsmData")?;

                if let Some(ref idx) = meta.index {
                    let has_match = match idx.kind {
                        crate::blob_meta::ElemKind::Node =>
                            ids.node_ids.any_in_range(idx.min_id, idx.max_id),
                        crate::blob_meta::ElemKind::Way =>
                            ids.way_ids.any_in_range(idx.min_id, idx.max_id),
                        crate::blob_meta::ElemKind::Relation =>
                            ids.relation_ids.any_in_range(idx.min_id, idx.max_id),
                    };
                    if include {
                        if !blob_filter.wants_index(idx) || !has_match {
                            blobs_skipped += 1;
                            continue;
                        }
                    } else if !has_match {
                        // Invert mode, no ID match: raw passthrough. We
                        // need the full frame bytes (length prefix +
                        // header + data) to write verbatim.
                        walker.pread_data(meta.frame_start, meta.frame_size, &mut frame_buf)?;
                        match idx.kind {
                            crate::blob_meta::ElemKind::Node => stats.nodes_written += idx.count,
                            crate::blob_meta::ElemKind::Way => stats.ways_written += idx.count,
                            crate::blob_meta::ElemKind::Relation => stats.relations_written += idx.count,
                        }
                        w.write_raw_owned(std::mem::take(&mut frame_buf))?;
                        blobs_passthrough += 1;
                        continue;
                    }
                }
                // Blob might contain matching IDs, or the blob carries no
                // indexdata and we must decode to check. Pread the data
                // and run the per-element filter.
                walker.pread_data(meta.data_offset, meta.data_size, &mut data_buf)?;
                decompress_buf.clear();
                crate::blob::decompress_blob_data_into(&data_buf, &mut decompress_buf)?;
                let block = PrimitiveBlock::new_with_scratch(
                    std::mem::take(&mut decompress_buf).into(),
                    &mut st_scratch, &mut gr_scratch,
                )?;
                output_blocks.clear();
                let (nodes, ways, relations) = process_block(
                    &block, &mut bb, &mut output_blocks, ids, include, None, false,
                ).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                flush_local(&mut bb, &mut output_blocks)
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                for (block_bytes, index, tagdata) in output_blocks.drain(..) {
                    w.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                }
                stats.nodes_written += nodes;
                stats.ways_written += ways;
                stats.relations_written += relations;
            }
            _ => {}
        }
    }

    let mut writer = writer.ok_or("no OSMHeader blob found")?;
    if blobs_skipped > 0 {
        eprintln!("[getid] {blobs_skipped} blobs skipped by ID range filter");
    }
    if blobs_passthrough > 0 {
        eprintln!("[getid --invert] {blobs_passthrough} blobs passed through raw");
    }
    writer.flush()?;
    crate::debug::emit_marker("GETID_SCAN_END");
    Ok(stats)
}


// ---------------------------------------------------------------------------
// Two-pass getid with --add-referenced
// ---------------------------------------------------------------------------

fn getid_with_refs(input: &Path, output: &Path, ids: &ElementIds, opts: &GetidOptions, compression: Compression, direct_io: bool, overrides: &HeaderOverrides) -> Result<GetidStats> {
    let mut stats = GetidStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    // Pass 1: Collect ref node IDs from matching ways. Uses IdSet for O(1)
    // lookups in pass 2 instead of BTreeSet's O(log n).
    crate::debug::emit_marker("GETID_PASS1_START");
    let mut dep_node_ids = crate::idset::IdSet::new();
    let mut has_dep_nodes = false;

    if ids.way_ids.has_any() {
        // Parallel classification: pread workers scan way blobs for matching
        // way IDs and collect their node refs.
        let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
            input, Some(crate::blob_meta::ElemKind::Way),
        )?;

        crate::scan::classify::parallel_classify_accumulate(
            &shared_file,
            &schedule,
            None,
            crate::idset::IdSet::new,
            |block, node_ids| {
                for element in block.elements_skip_metadata() {
                    if let Element::Way(w) = &element
                        && ids.way_ids.get(w.id())
                    {
                        for r in w.refs() { node_ids.set(r); }
                    }
                }
            },
            |worker_node_ids| {
                if worker_node_ids.has_any() { has_dep_nodes = true; }
                dep_node_ids.merge(worker_node_ids);
            },
        )?;
    }
    // When --remove-tags is set, referenced-only nodes (not explicitly requested)
    // get their tags stripped. Check at query time: dep_node_ids.get(id) && !ids.node_ids.get(id).
    let strip_tags = opts.remove_tags && has_dep_nodes;
    crate::debug::emit_marker("GETID_PASS1_END");
    // Cheap one-time iteration; gives the dep-set size before pass 2
    // starts so the counter survives a SIGKILL during pass 2.
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "getid_dep_node_ids",
            i64::try_from(dep_node_ids.iter().count()).unwrap_or(i64::MAX),
        );
    }

    // Pass 2: Write matching elements + dependent nodes (parallel batches).
    crate::debug::emit_marker("GETID_PASS2_START");
    let reader = ElementReader::open(input, direct_io)?;
    super::warn_locations_on_ways_loss(reader.header());
    // Skip blob types not needed: nodes if no node IDs and no dependent nodes,
    // ways always needed (add-referenced mode), relations if no relation IDs.
    let reader = reader.with_blob_filter(BlobFilter::new(
        ids.node_ids.has_any() || has_dep_nodes,
        true,
        ids.relation_ids.has_any(),
    ));
    let mut writer = writer_from_header(output, compression, reader.header(), true, overrides, |hb| hb, direct_io, false)?;

    let dep_ref = if has_dep_nodes { Some(&dep_node_ids) } else { None };

    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
            let (nodes, ways, relations) = process_filter_batch(
                batch, &mut writer, ids, true, dep_ref, strip_tags,
            )?;
            stats.nodes_written += nodes;
            stats.ways_written += ways;
            stats.relations_written += relations;
            Ok(())
        })?;

    writer.flush()?;
    crate::debug::emit_marker("GETID_PASS2_END");
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
    ids: &ElementIds,
    include: bool,
    dep_node_ids: Option<&crate::idset::IdSet>,
    strip_tags: bool,
) -> std::result::Result<(u64, u64, u64), String> {
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;

    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        let dominated = match &element {
            Element::DenseNode(dn) => {
                ids.node_ids.get(dn.id())
                    || dep_node_ids.is_some_and(|deps| deps.get(dn.id()))
            }
            Element::Node(n) => {
                ids.node_ids.get(n.id())
                    || dep_node_ids.is_some_and(|deps| deps.get(n.id()))
            }
            Element::Way(w) => ids.way_ids.get(w.id()),
            Element::Relation(r) => ids.relation_ids.get(r.id()),
        };
        let emit = if include { dominated } else { !dominated };
        if !emit {
            continue;
        }

        match &element {
            Element::DenseNode(dn) => {
                ensure_node_capacity_local(bb, output)?;
                // Strip tags from referenced-only nodes (dep but not explicit)
                let strip = strip_tags
                    && dep_node_ids.is_some_and(|deps| deps.get(dn.id()))
                    && !ids.node_ids.get(dn.id());
                let meta = dense_node_metadata(dn);
                if strip {
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), std::iter::empty::<(&str, &str)>(), meta.as_ref());
                } else {
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
                }
                nodes += 1;
            }
            Element::Node(n) => {
                ensure_node_capacity_local(bb, output)?;
                let strip = strip_tags
                    && dep_node_ids.is_some_and(|deps| deps.get(n.id()))
                    && !ids.node_ids.get(n.id());
                let meta = element_metadata(&n.info());
                if strip {
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), std::iter::empty::<(&str, &str)>(), meta.as_ref());
                } else {
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
                }
                nodes += 1;
            }
            Element::Way(w) => {
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = element_metadata(&w.info());
                bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                ways += 1;
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
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
    ids: &ElementIds,
    include: bool,
    dep_node_ids: Option<&crate::idset::IdSet>,
    strip_tags: bool,
) -> Result<(u64, u64, u64)> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, (u64, u64, u64)), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let (nodes, ways, relations) = process_block(
                    block, bb, &mut output, ids, include, dep_node_ids, strip_tags,
                )?;
                flush_local(bb, &mut output)?;
                Ok((output, (nodes, ways, relations)))
            },
        )
        .collect();

    let mut total_nodes: u64 = 0;
    let mut total_ways: u64 = 0;
    let mut total_relations: u64 = 0;
    drain_batch_results(results, writer, |(nodes, ways, relations)| {
        total_nodes += nodes;
        total_ways += ways;
        total_relations += relations;
    })?;

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
        let (prefix, id) = parse_id_spec("n123", None).unwrap();
        assert_eq!(prefix, 'n');
        assert_eq!(id, 123);
    }

    #[test]
    fn parse_way_id() {
        let (prefix, id) = parse_id_spec("w456", None).unwrap();
        assert_eq!(prefix, 'w');
        assert_eq!(id, 456);
    }

    #[test]
    fn parse_relation_id() {
        let (prefix, id) = parse_id_spec("r789", None).unwrap();
        assert_eq!(prefix, 'r');
        assert_eq!(id, 789);
    }

    #[test]
    fn parse_large_id() {
        let (prefix, id) = parse_id_spec("n9876543210", None).unwrap();
        assert_eq!(prefix, 'n');
        assert_eq!(id, 9_876_543_210);
    }

    #[test]
    fn parse_invalid_prefix() {
        assert!(parse_id_spec("x123", None).is_err());
    }

    #[test]
    fn parse_missing_number() {
        assert!(parse_id_spec("n", None).is_err());
    }

    #[test]
    fn parse_bad_number() {
        assert!(parse_id_spec("nabc", None).is_err());
    }

    #[test]
    fn parse_too_short() {
        assert!(parse_id_spec("n", None).is_err());
        assert!(parse_id_spec("", None).is_err());
    }

    #[test]
    fn parse_ids_mixed() {
        let specs: Vec<String> = vec!["n1", "n2", "w10", "r100"]
            .into_iter()
            .map(ToString::to_string)
            .collect();
        let set = parse_ids(&specs).unwrap();
        assert!(set.node_ids.get(1));
        assert!(set.node_ids.get(2));
        assert!(!set.node_ids.get(3));
        assert!(set.way_ids.get(10));
        assert!(set.relation_ids.get(100));
    }

    #[test]
    fn parse_bare_id_with_default_type_node() {
        let (prefix, id) = parse_id_spec("42", Some(DefaultType::Node)).unwrap();
        assert_eq!(prefix, 'n');
        assert_eq!(id, 42);
    }

    #[test]
    fn parse_ids_bare_with_default_type_way() {
        let specs: Vec<String> = vec!["1", "2", "w10", "r100"]
            .into_iter()
            .map(ToString::to_string)
            .collect();
        let set = parse_ids_with_default_type(&specs, Some(DefaultType::Way)).unwrap();
        assert!(!set.node_ids.has_any());
        assert!(set.way_ids.get(1));
        assert!(set.way_ids.get(2));
        assert!(set.way_ids.get(10));
        assert!(set.relation_ids.get(100));
    }

    #[test]
    fn parse_ids_bare_without_default_type_errors() {
        let specs: Vec<String> = vec!["123".to_string()];
        assert!(parse_ids_with_default_type(&specs, None).is_err());
    }

    #[test]
    fn parse_negative_id_rejected_with_named_id_and_kind() {
        let err = parse_id_spec("n-1", None).unwrap_err().to_string();
        assert!(err.contains("non-negative"), "{err}");
        assert!(err.contains("node id -1"), "{err}");

        let err = parse_id_spec("w-42", None).unwrap_err().to_string();
        assert!(err.contains("way id -42"), "{err}");

        let err = parse_id_spec("r-7", None).unwrap_err().to_string();
        assert!(err.contains("relation id -7"), "{err}");

        // Bare negative number with default type must also reject.
        let err = parse_id_spec("-5", Some(DefaultType::Node)).unwrap_err().to_string();
        assert!(err.contains("node id -5"), "{err}");
    }
}
