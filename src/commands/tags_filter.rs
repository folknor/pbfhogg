//! Filter elements by tag expressions. Equivalent to `osmium tags-filter`.

use std::path::Path;

use rayon::prelude::*;

use super::{dense_node_metadata, element_metadata};
use crate::block_builder::{HeaderBuilder, BlockBuilder, MemberData};
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, PrimitiveBlock};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Expression types
// ---------------------------------------------------------------------------

/// Which element types an expression applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// What to match on a tag.
#[derive(Clone, Debug)]
enum TagMatcher {
    /// Key exists (any value): `amenity`
    KeyOnly { key: String },
    /// Key matches prefix with wildcard: `addr:*`
    KeyPrefix { prefix: String },
    /// Key=value exact match: `highway=primary`
    ExactValue { key: String, value: String },
    /// Key=val1,val2,... (any of the values): `type=multipolygon,boundary`
    MultiValue { key: String, values: Vec<String> },
    /// Key!=value (key exists but value differs): `highway!=primary`
    NotValue { key: String, value: String },
}

/// A parsed filter expression.
#[derive(Clone, Debug)]
struct Expression {
    type_filter: TypeFilter,
    matcher: TagMatcher,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

fn parse_type_prefix(input: &str) -> (TypeFilter, &str) {
    if let Some(slash_pos) = input.find('/') {
        let prefix = &input[..slash_pos];
        let rest = &input[slash_pos + 1..];
        if !prefix.is_empty() && prefix.chars().all(|c| matches!(c, 'n' | 'w' | 'r')) {
            let tf = TypeFilter {
                nodes: prefix.contains('n'),
                ways: prefix.contains('w'),
                relations: prefix.contains('r'),
            };
            return (tf, rest);
        }
    }
    (TypeFilter::all(), input)
}

fn parse_tag_matcher(input: &str) -> Result<TagMatcher> {
    // Check != before = to avoid ambiguity
    if let Some(pos) = input.find("!=") {
        let key = &input[..pos];
        let value = &input[pos + 2..];
        if key.is_empty() {
            return Err("empty key in negation expression".into());
        }
        return Ok(TagMatcher::NotValue {
            key: key.to_string(),
            value: value.to_string(),
        });
    }
    if let Some(pos) = input.find('=') {
        let key = &input[..pos];
        let value_part = &input[pos + 1..];
        if key.is_empty() {
            return Err("empty key in expression".into());
        }
        if value_part.contains(',') {
            let values: Vec<String> = value_part.split(',').map(ToString::to_string).collect();
            return Ok(TagMatcher::MultiValue {
                key: key.to_string(),
                values,
            });
        }
        return Ok(TagMatcher::ExactValue {
            key: key.to_string(),
            value: value_part.to_string(),
        });
    }
    // Wildcard key prefix: `addr:*`
    if input.ends_with(":*") {
        let prefix = &input[..input.len() - 1]; // keep the colon, strip the *
        return Ok(TagMatcher::KeyPrefix {
            prefix: prefix.to_string(),
        });
    }
    if input.is_empty() {
        return Err("empty expression".into());
    }
    Ok(TagMatcher::KeyOnly {
        key: input.to_string(),
    })
}

fn parse_expression(input: &str) -> Result<Expression> {
    let (type_filter, tag_part) = parse_type_prefix(input);
    let matcher = parse_tag_matcher(tag_part)?;
    Ok(Expression {
        type_filter,
        matcher,
    })
}

fn parse_expressions(inputs: &[String]) -> Result<Vec<Expression>> {
    inputs.iter().map(|s| parse_expression(s)).collect()
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

fn tag_matches(matcher: &TagMatcher, key: &str, value: &str) -> bool {
    match matcher {
        TagMatcher::KeyOnly { key: k } => key == k,
        TagMatcher::KeyPrefix { prefix } => key.starts_with(prefix.as_str()),
        TagMatcher::ExactValue { key: k, value: v } => key == k && value == v,
        TagMatcher::MultiValue { key: k, values } => {
            key == k && values.iter().any(|v| v == value)
        }
        TagMatcher::NotValue { key: k, value: v } => key == k && value != v,
    }
}

/// Compute a `BlobFilter` from the union of all expression type filters.
/// Returns `None` if all types are needed (no benefit from filtering).
fn blob_filter_from_expressions(expressions: &[Expression]) -> Option<BlobFilter> {
    let mut need_nodes = false;
    let mut need_ways = false;
    let mut need_relations = false;
    for expr in expressions {
        need_nodes |= expr.type_filter.nodes;
        need_ways |= expr.type_filter.ways;
        need_relations |= expr.type_filter.relations;
    }
    if need_nodes && need_ways && need_relations {
        None
    } else {
        Some(BlobFilter::new(need_nodes, need_ways, need_relations))
    }
}

/// Check if an element's tags match any applicable expression (OR semantics).
fn element_matches(
    expressions: &[Expression],
    tags: &[(&str, &str)],
    is_node: bool,
    is_way: bool,
    is_relation: bool,
) -> bool {
    for expr in expressions {
        let type_ok = (is_node && expr.type_filter.nodes)
            || (is_way && expr.type_filter.ways)
            || (is_relation && expr.type_filter.relations);
        if !type_ok {
            continue;
        }
        for &(key, value) in tags {
            if tag_matches(&expr.matcher, key, value) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from a tags-filter operation.
pub struct TagsFilterStats {
    pub nodes_matched: u64,
    pub nodes_from_ways: u64,
    pub ways_matched: u64,
    pub relations_matched: u64,
}

impl TagsFilterStats {
    pub fn print_summary(&self) {
        let total = self.nodes_matched
            + self.nodes_from_ways
            + self.ways_matched
            + self.relations_matched;
        eprintln!(
            "Wrote {total} elements: {} nodes ({} direct + {} from ways), {} ways, {} relations",
            self.nodes_matched + self.nodes_from_ways,
            self.nodes_matched,
            self.nodes_from_ways,
            self.ways_matched,
            self.relations_matched,
        );
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Filter a PBF file by tag expressions.
///
/// If `omit_referenced` is true (`-R` flag), only directly matching elements
/// are output (single pass, faster). Otherwise, referenced nodes of matching
/// ways are also included (two-pass).
#[hotpath::measure]
pub fn tags_filter(
    input: &Path,
    output: &Path,
    expression_strs: &[String],
    omit_referenced: bool,
    compression: Compression,
    direct_io: bool,
) -> Result<TagsFilterStats> {
    let expressions = parse_expressions(expression_strs)?;
    if omit_referenced {
        tags_filter_single_pass(input, output, &expressions, compression, direct_io)
    } else {
        tags_filter_two_pass(input, output, &expressions, compression, direct_io)
    }
}

// ---------------------------------------------------------------------------
// Single-pass filter (-R mode)
// ---------------------------------------------------------------------------

const BATCH_SIZE: usize = 64;

fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<Vec<u8>>) -> std::result::Result<(), String> {
    if let Some(bytes) = bb.take().map_err(|e| e.to_string())? {
        output.push(bytes.to_vec());
    }
    Ok(())
}

/// Process a single block through tag-filter expressions on a rayon thread.
/// Returns per-block stats.
fn filter_block_parallel(
    block: &PrimitiveBlock,
    expressions: &[Expression],
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
) -> std::result::Result<TagsFilterStats, String> {
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        ways_matched: 0,
        relations_matched: 0,
    };
    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                tags_buf.clear();
                tags_buf.extend(dn.tags());
                if element_matches(expressions, &tags_buf, true, false, false) {
                    if !bb.can_add_node() {
                        flush_local(bb, output)?;
                    }
                    let meta = dense_node_metadata(dn);
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        &tags_buf,
                        meta.as_ref(),
                    );
                    stats.nodes_matched += 1;
                }
            }
            Element::Node(n) => {
                tags_buf.clear();
                tags_buf.extend(n.tags());
                if element_matches(expressions, &tags_buf, true, false, false) {
                    if !bb.can_add_node() {
                        flush_local(bb, output)?;
                    }
                    let meta = element_metadata(&n.info());
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        &tags_buf,
                        meta.as_ref(),
                    );
                    stats.nodes_matched += 1;
                }
            }
            Element::Way(w) => {
                tags_buf.clear();
                tags_buf.extend(w.tags());
                if element_matches(expressions, &tags_buf, false, true, false) {
                    if !bb.can_add_way() {
                        flush_local(bb, output)?;
                    }
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                    stats.ways_matched += 1;
                }
            }
            Element::Relation(r) => {
                tags_buf.clear();
                tags_buf.extend(r.tags());
                if element_matches(expressions, &tags_buf, false, false, true) {
                    if !bb.can_add_relation() {
                        flush_local(bb, output)?;
                    }
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                    stats.relations_matched += 1;
                }
            }
        }
    }
    Ok(stats)
}

#[allow(clippy::too_many_lines)]
fn tags_filter_single_pass(
    input: &Path,
    output: &Path,
    expressions: &[Expression],
    compression: Compression,
    direct_io: bool,
) -> Result<TagsFilterStats> {
    let reader = ElementReader::open(input, direct_io)?;
    let reader = match blob_filter_from_expressions(expressions) {
        Some(filter) => reader.with_blob_filter(filter),
        None => reader,
    };
    let mut hb = HeaderBuilder::from_header(reader.header());
    if reader.header().is_sorted() {
        hb = hb.sorted();
    }
    let header_bytes = hb.build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        ways_matched: 0,
        relations_matched: 0,
    };

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    for block_result in reader.into_blocks_pipelined() {
        batch.push(block_result?);
        if batch.len() >= BATCH_SIZE {
            process_filter_batch(&batch, expressions, &mut writer, &mut stats)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_filter_batch(&batch, expressions, &mut writer, &mut stats)?;
    }

    writer.flush()?;
    Ok(stats)
}

/// Process a batch of blocks in parallel for single-pass tag filtering.
fn process_filter_batch(
    batch: &[PrimitiveBlock],
    expressions: &[Expression],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut TagsFilterStats,
) -> Result<()> {
    type BatchResult = std::result::Result<(Vec<Vec<u8>>, TagsFilterStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<Vec<u8>> = Vec::new();
                let block_stats = filter_block_parallel(block, expressions, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    for result in results {
        let (blocks, block_stats) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        stats.nodes_matched += block_stats.nodes_matched;
        stats.ways_matched += block_stats.ways_matched;
        stats.relations_matched += block_stats.relations_matched;
        for block_bytes in &blocks {
            writer.write_primitive_block(block_bytes)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Two-pass filter: Pass 2 parallel helpers
// ---------------------------------------------------------------------------

/// Read-only ID sets for Pass 2, shared across rayon threads.
struct Pass2IdSets<'a> {
    matched_node_ids: &'a [i64],
    matched_way_ids: &'a [i64],
    matched_relation_ids: &'a [i64],
    way_dep_node_ids: &'a [i64],
}

/// Process a single block in Pass 2: write elements whose IDs were collected in Pass 1.
fn filter_block_pass2(
    block: &PrimitiveBlock,
    ids: &Pass2IdSets<'_>,
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
) -> std::result::Result<TagsFilterStats, String> {
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        ways_matched: 0,
        relations_matched: 0,
    };
    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let direct = ids.matched_node_ids.binary_search(&dn.id()).is_ok();
                let from_way = ids.way_dep_node_ids.binary_search(&dn.id()).is_ok();
                if direct || from_way {
                    if !bb.can_add_node() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = dense_node_metadata(dn);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags_buf, meta.as_ref());
                    if direct { stats.nodes_matched += 1; } else { stats.nodes_from_ways += 1; }
                }
            }
            Element::Node(n) => {
                let direct = ids.matched_node_ids.binary_search(&n.id()).is_ok();
                let from_way = ids.way_dep_node_ids.binary_search(&n.id()).is_ok();
                if direct || from_way {
                    if !bb.can_add_node() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                    if direct { stats.nodes_matched += 1; } else { stats.nodes_from_ways += 1; }
                }
            }
            Element::Way(w) => {
                if ids.matched_way_ids.binary_search(&w.id()).is_ok() {
                    if !bb.can_add_way() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                    stats.ways_matched += 1;
                }
            }
            Element::Relation(r) => {
                if ids.matched_relation_ids.binary_search(&r.id()).is_ok() {
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
                    stats.relations_matched += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Process a batch of blocks in parallel for Pass 2 of two-pass tag filtering.
fn process_pass2_batch(
    batch: &[PrimitiveBlock],
    ids: &Pass2IdSets<'_>,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut TagsFilterStats,
) -> Result<()> {
    type BatchResult = std::result::Result<(Vec<Vec<u8>>, TagsFilterStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<Vec<u8>> = Vec::new();
                let block_stats = filter_block_pass2(block, ids, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    for result in results {
        let (blocks, block_stats) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        stats.nodes_matched += block_stats.nodes_matched;
        stats.nodes_from_ways += block_stats.nodes_from_ways;
        stats.ways_matched += block_stats.ways_matched;
        stats.relations_matched += block_stats.relations_matched;
        for block_bytes in &blocks {
            writer.write_primitive_block(block_bytes)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Two-pass filter (default mode, include references)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn tags_filter_two_pass(
    input: &Path,
    output: &Path,
    expressions: &[Expression],
    compression: Compression,
    direct_io: bool,
) -> Result<TagsFilterStats> {
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        ways_matched: 0,
        relations_matched: 0,
    };

    // --- Pass 1: Collect match results and way dependencies ---
    //
    // OPTIMIZATION: Use Vec<i64> instead of BTreeSet<i64> for matched element IDs.
    //
    // Previously these were BTreeSet<i64>, which stores each entry in a B-tree node
    // with ~40 bytes overhead per entry (node pointers, balance metadata, alignment
    // padding). For large tag filters on planet-scale files with millions of matched
    // elements, this overhead dominates memory usage.
    //
    // Sorted Vec<i64> uses exactly 8 bytes per entry (just the i64 itself), giving
    // a ~5x memory reduction. Lookups use binary_search() which is O(log n) -- the
    // same asymptotic complexity as BTreeSet::contains() -- but with much better
    // cache locality since the data is stored contiguously in memory rather than
    // scattered across heap-allocated tree nodes.
    //
    // The pattern is straightforward here because tags_filter_two_pass has a clean
    // separation: pass 1 ONLY inserts IDs (no lookups within pass 1), and pass 2
    // ONLY does lookups. The sort+dedup happens in the gap between passes.
    //
    // Alternatives considered:
    // - HashSet<i64>: Even worse memory overhead (~72 bytes/entry due to hash table
    //   bucket array, load factor headroom, and per-entry hash + metadata storage).
    // - roaring::RoaringBitmap: Excellent compression for dense ID ranges, but adds
    //   an external dependency. Overkill for typical filter result sizes where the
    //   simple sorted Vec approach is sufficient.
    //
    // sort_unstable() is used instead of sort() because i64 has no meaningful
    // stability requirement (equal elements are identical), and sort_unstable()
    // avoids the temporary allocation that sort() needs for its merge step.
    let mut matched_node_ids: Vec<i64> = Vec::new();
    let mut matched_way_ids: Vec<i64> = Vec::new();
    let mut matched_relation_ids: Vec<i64> = Vec::new();
    let mut way_dep_node_ids: Vec<i64> = Vec::new();

    // Pass 1: skip blob types that no expression targets.
    let reader = ElementReader::open(input, direct_io)?;
    let reader = match blob_filter_from_expressions(expressions) {
        Some(filter) => reader.with_blob_filter(filter),
        None => reader,
    };
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        let mut tags_buf: Vec<(&str, &str)> = Vec::new();
        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => {
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    if element_matches(expressions, &tags_buf, true, false, false) {
                        matched_node_ids.push(dn.id());
                    }
                }
                Element::Node(n) => {
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    if element_matches(expressions, &tags_buf, true, false, false) {
                        matched_node_ids.push(n.id());
                    }
                }
                Element::Way(w) => {
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    if element_matches(expressions, &tags_buf, false, true, false) {
                        matched_way_ids.push(w.id());
                        way_dep_node_ids.extend(w.refs());
                    }
                }
                Element::Relation(r) => {
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
                    if element_matches(expressions, &tags_buf, false, false, true) {
                        matched_relation_ids.push(r.id());
                    }
                }
            }
        }
    }

    // Sort and deduplicate all ID Vecs between pass 1 and pass 2. This is the key
    // step that converts the unsorted append-only Vecs into sorted arrays suitable
    // for binary_search() lookups in pass 2.
    //
    // sort_unstable() is preferred over sort() for primitive types: no stability is
    // needed (equal i64 values are identical), and it avoids the temporary buffer
    // allocation that sort()'s merge step requires.
    //
    // dedup() removes consecutive duplicates (requires prior sorting). Duplicates
    // can arise from the same element appearing in multiple blocks, or from
    // way_dep_node_ids collecting the same node ref from multiple matching ways.
    matched_node_ids.sort_unstable();
    matched_node_ids.dedup();
    matched_way_ids.sort_unstable();
    matched_way_ids.dedup();
    matched_relation_ids.sort_unstable();
    matched_relation_ids.dedup();
    way_dep_node_ids.sort_unstable();
    way_dep_node_ids.dedup();

    // --- Pass 2: Write matching elements in file order (parallel batches) ---
    // Pass 2 always needs nodes (for way deps) plus matched ways/relations.
    let reader = ElementReader::open(input, direct_io)?;
    let reader = if let Some(filter) = blob_filter_from_expressions(expressions) {
        // Ensure nodes are always included — matched ways pull in referenced nodes.
        reader.with_blob_filter(BlobFilter::new(true, filter.want_ways, filter.want_relations))
    } else {
        reader
    };
    let mut hb = HeaderBuilder::from_header(reader.header());
    if reader.header().is_sorted() {
        hb = hb.sorted();
    }
    let header_bytes = hb.build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;

    let id_sets = Pass2IdSets {
        matched_node_ids: &matched_node_ids,
        matched_way_ids: &matched_way_ids,
        matched_relation_ids: &matched_relation_ids,
        way_dep_node_ids: &way_dep_node_ids,
    };

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    for block in reader.into_blocks_pipelined() {
        batch.push(block?);
        if batch.len() >= BATCH_SIZE {
            process_pass2_batch(&batch, &id_sets, &mut writer, &mut stats)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_pass2_batch(&batch, &id_sets, &mut writer, &mut stats)?;
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------


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
    fn parse_key_only() {
        let expr = parse_expression("amenity").unwrap();
        assert_eq!(expr.type_filter, TypeFilter::all());
        assert!(matches!(
            expr.matcher,
            TagMatcher::KeyOnly { ref key } if key == "amenity"
        ));
    }

    #[test]
    fn parse_exact_value() {
        let expr = parse_expression("highway=primary").unwrap();
        assert!(matches!(
            expr.matcher,
            TagMatcher::ExactValue { ref key, ref value }
                if key == "highway" && value == "primary"
        ));
    }

    #[test]
    fn parse_multi_value() {
        let expr = parse_expression("type=multipolygon,boundary").unwrap();
        assert!(matches!(
            expr.matcher,
            TagMatcher::MultiValue { ref key, ref values }
                if key == "type" && values == &["multipolygon", "boundary"]
        ));
    }

    #[test]
    fn parse_negation() {
        let expr = parse_expression("highway!=primary").unwrap();
        assert!(matches!(
            expr.matcher,
            TagMatcher::NotValue { ref key, ref value }
                if key == "highway" && value == "primary"
        ));
    }

    #[test]
    fn parse_wildcard_prefix() {
        let expr = parse_expression("addr:*").unwrap();
        assert!(matches!(
            expr.matcher,
            TagMatcher::KeyPrefix { ref prefix } if prefix == "addr:"
        ));
    }

    #[test]
    fn parse_type_prefix_node() {
        let expr = parse_expression("n/amenity").unwrap();
        assert!(expr.type_filter.nodes);
        assert!(!expr.type_filter.ways);
        assert!(!expr.type_filter.relations);
    }

    #[test]
    fn parse_type_prefix_nw() {
        let expr = parse_expression("nw/highway=primary").unwrap();
        assert!(expr.type_filter.nodes);
        assert!(expr.type_filter.ways);
        assert!(!expr.type_filter.relations);
    }

    #[test]
    fn parse_type_prefix_nwr() {
        let expr = parse_expression("nwr/name").unwrap();
        assert_eq!(expr.type_filter, TypeFilter::all());
    }

    #[test]
    fn parse_slash_in_key_not_type_prefix() {
        // "addr:full/name" has non-nwr chars before '/', so no type prefix
        let expr = parse_expression("addr:full/name").unwrap();
        assert_eq!(expr.type_filter, TypeFilter::all());
        assert!(matches!(
            expr.matcher,
            TagMatcher::KeyOnly { ref key } if key == "addr:full/name"
        ));
    }

    #[test]
    fn parse_empty_is_error() {
        assert!(parse_expression("").is_err());
    }

    #[test]
    fn parse_empty_key_in_value_expr_is_error() {
        assert!(parse_expression("=value").is_err());
    }

    #[test]
    fn parse_empty_key_in_negation_is_error() {
        assert!(parse_expression("!=value").is_err());
    }

    #[test]
    fn match_key_only() {
        let m = TagMatcher::KeyOnly {
            key: "amenity".to_string(),
        };
        assert!(tag_matches(&m, "amenity", "restaurant"));
        assert!(tag_matches(&m, "amenity", "bench"));
        assert!(!tag_matches(&m, "highway", "primary"));
    }

    #[test]
    fn match_exact_value() {
        let m = TagMatcher::ExactValue {
            key: "highway".to_string(),
            value: "primary".to_string(),
        };
        assert!(tag_matches(&m, "highway", "primary"));
        assert!(!tag_matches(&m, "highway", "secondary"));
        assert!(!tag_matches(&m, "amenity", "primary"));
    }

    #[test]
    fn match_multi_value() {
        let m = TagMatcher::MultiValue {
            key: "type".to_string(),
            values: vec!["multipolygon".to_string(), "boundary".to_string()],
        };
        assert!(tag_matches(&m, "type", "multipolygon"));
        assert!(tag_matches(&m, "type", "boundary"));
        assert!(!tag_matches(&m, "type", "route"));
    }

    #[test]
    fn match_not_value() {
        let m = TagMatcher::NotValue {
            key: "highway".to_string(),
            value: "primary".to_string(),
        };
        assert!(tag_matches(&m, "highway", "secondary"));
        assert!(!tag_matches(&m, "highway", "primary"));
        assert!(!tag_matches(&m, "amenity", "bench"));
    }

    #[test]
    fn match_key_prefix() {
        let m = TagMatcher::KeyPrefix {
            prefix: "addr:".to_string(),
        };
        assert!(tag_matches(&m, "addr:street", "Main St"));
        assert!(tag_matches(&m, "addr:city", "Berlin"));
        assert!(!tag_matches(&m, "name", "foo"));
    }

    #[test]
    fn element_matches_respects_type_filter() {
        let expr = Expression {
            type_filter: TypeFilter {
                nodes: false,
                ways: true,
                relations: false,
            },
            matcher: TagMatcher::KeyOnly {
                key: "highway".to_string(),
            },
        };
        let tags = [("highway", "primary")];
        assert!(!element_matches(std::slice::from_ref(&expr), &tags, true, false, false));
        assert!(element_matches(std::slice::from_ref(&expr), &tags, false, true, false));
        assert!(!element_matches(std::slice::from_ref(&expr), &tags, false, false, true));
    }

    #[test]
    fn element_matches_or_semantics() {
        let exprs = vec![
            Expression {
                type_filter: TypeFilter::all(),
                matcher: TagMatcher::KeyOnly {
                    key: "amenity".to_string(),
                },
            },
            Expression {
                type_filter: TypeFilter::all(),
                matcher: TagMatcher::ExactValue {
                    key: "highway".to_string(),
                    value: "primary".to_string(),
                },
            },
        ];
        assert!(element_matches(&exprs, &[("amenity", "bench")], true, false, false));
        assert!(element_matches(
            &exprs,
            &[("highway", "primary")],
            false,
            true,
            false
        ));
        assert!(!element_matches(
            &exprs,
            &[("name", "foo")],
            true,
            false,
            false
        ));
    }
}
