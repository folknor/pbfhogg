//! Filter elements by tag expressions. Equivalent to `osmium tags-filter`.

use std::path::Path;

use rayon::prelude::*;

use super::id_set_dense::IdSetDense;
use super::{
    dense_node_metadata, drain_batch_results, element_metadata, flush_local, require_indexdata,
    for_each_primitive_block_batch, writer_from_header, TypeFilter,
    ensure_node_capacity_local, ensure_way_capacity_local, ensure_relation_capacity_local,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, MemberId, PrimitiveBlock};

use super::{Result, BATCH_SIZE};

// ---------------------------------------------------------------------------
// Expression types
// ---------------------------------------------------------------------------

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

/// Compute a `BlobFilter` from the union of all expression type + tag key filters.
///
/// Returns `None` only if all types are needed AND no tag keys can be extracted
/// (no benefit from filtering at blob level).
fn blob_filter_from_expressions(expressions: &[Expression]) -> Option<BlobFilter> {
    let mut need_nodes = false;
    let mut need_ways = false;
    let mut need_relations = false;
    let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut prefixes: std::collections::HashSet<String> = std::collections::HashSet::new();

    for expr in expressions {
        need_nodes |= expr.type_filter.nodes;
        need_ways |= expr.type_filter.ways;
        need_relations |= expr.type_filter.relations;

        match &expr.matcher {
            TagMatcher::KeyOnly { key } => { keys.insert(key.clone()); }
            TagMatcher::KeyPrefix { prefix } => { prefixes.insert(prefix.clone()); }
            TagMatcher::ExactValue { key, .. }
            | TagMatcher::MultiValue { key, .. }
            | TagMatcher::NotValue { key, .. } => { keys.insert(key.clone()); }
        }
    }

    let all_types = need_nodes && need_ways && need_relations;
    let has_tag_filter = !keys.is_empty() || !prefixes.is_empty();

    if all_types && !has_tag_filter {
        return None;
    }

    let mut filter = BlobFilter::new(need_nodes, need_ways, need_relations);
    if !keys.is_empty() {
        filter = filter.with_required_tag_keys(keys.into_iter().collect());
    }
    if !prefixes.is_empty() {
        filter = filter.with_required_tag_prefixes(prefixes.into_iter().collect());
    }
    Some(filter)
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
    /// Directly tag-matched nodes.
    pub nodes_matched: u64,
    /// Nodes included because they are referenced by directly matched ways.
    pub nodes_from_ways: u64,
    /// Nodes included through relation-member dependency expansion.
    pub nodes_from_relations: u64,
    /// Directly tag-matched ways.
    pub ways_matched: u64,
    /// Ways included through relation-member dependency expansion.
    pub ways_from_relations: u64,
    /// Directly tag-matched relations.
    pub relations_matched: u64,
    /// Relations included through transitive relation-member closure.
    pub relations_from_relations: u64,
}

impl TagsFilterStats {
    pub fn print_summary(&self) {
        let total = self.nodes_matched
            + self.nodes_from_ways
            + self.nodes_from_relations
            + self.ways_matched
            + self.ways_from_relations
            + self.relations_matched
            + self.relations_from_relations;
        eprintln!(
            "Wrote {total} elements: {} nodes ({} direct + {} from ways + {} from relations), \
             {} ways ({} direct + {} from relations), {} relations ({} direct + {} from relations)",
            self.nodes_matched + self.nodes_from_ways + self.nodes_from_relations,
            self.nodes_matched,
            self.nodes_from_ways,
            self.nodes_from_relations,
            self.ways_matched,
            self.ways_matched,
            self.ways_from_relations,
            self.relations_matched,
            self.relations_matched,
            self.relations_from_relations,
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
    force: bool,
) -> Result<TagsFilterStats> {
    require_indexdata(input, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, type and tag key \
         filters are no-ops — all blobs are decompressed (significantly slower).")?;

    {
        let reader = crate::ElementReader::open(input, direct_io)?;
        super::warn_locations_on_ways_loss(reader.header());
    }

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


/// Process a single block through tag-filter expressions on a rayon thread.
/// Returns per-block stats.
fn filter_block_parallel(
    block: &PrimitiveBlock,
    expressions: &[Expression],
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<TagsFilterStats, String> {
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_matched: 0,
        ways_from_relations: 0,
        relations_matched: 0,
        relations_from_relations: 0,
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
                    ensure_node_capacity_local(bb, output)?;
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
                    ensure_node_capacity_local(bb, output)?;
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
                    ensure_way_capacity_local(bb, output)?;
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
                    ensure_relation_capacity_local(bb, output)?;
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
    let mut writer = writer_from_header(output, compression, reader.header(), true, |hb| hb)?;
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_matched: 0,
        ways_from_relations: 0,
        relations_matched: 0,
        relations_from_relations: 0,
    };

    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        process_filter_batch(batch, expressions, &mut writer, &mut stats)
    })?;

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
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, TagsFilterStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = filter_block_parallel(block, expressions, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    drain_batch_results(results, writer, |s| {
        stats.nodes_matched += s.nodes_matched;
        stats.ways_matched += s.ways_matched;
        stats.relations_matched += s.relations_matched;
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Two-pass filter: Pass 2 parallel helpers
// ---------------------------------------------------------------------------

/// Read-only ID sets for Pass 2, shared across rayon threads.
struct Pass2IdSets<'a> {
    matched_node_ids: &'a IdSetDense,
    direct_way_ids: &'a IdSetDense,
    included_way_ids: &'a IdSetDense,
    direct_relation_ids: &'a IdSetDense,
    included_relation_ids: &'a IdSetDense,
    way_dep_node_ids: &'a IdSetDense,
    relation_dep_node_ids: &'a IdSetDense,
}

/// Process a single block in Pass 2: write elements whose IDs were collected in Pass 1.
fn filter_block_pass2(
    block: &PrimitiveBlock,
    ids: &Pass2IdSets<'_>,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<TagsFilterStats, String> {
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_matched: 0,
        ways_from_relations: 0,
        relations_matched: 0,
        relations_from_relations: 0,
    };
    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let direct = ids.matched_node_ids.get(dn.id());
                let from_way = ids.way_dep_node_ids.get(dn.id());
                let from_relation = ids.relation_dep_node_ids.get(dn.id());
                if direct || from_way || from_relation {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = dense_node_metadata(dn);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags_buf, meta.as_ref());
                    if direct {
                        stats.nodes_matched += 1;
                    } else if from_way {
                        stats.nodes_from_ways += 1;
                    } else {
                        stats.nodes_from_relations += 1;
                    }
                }
            }
            Element::Node(n) => {
                let direct = ids.matched_node_ids.get(n.id());
                let from_way = ids.way_dep_node_ids.get(n.id());
                let from_relation = ids.relation_dep_node_ids.get(n.id());
                if direct || from_way || from_relation {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                    if direct {
                        stats.nodes_matched += 1;
                    } else if from_way {
                        stats.nodes_from_ways += 1;
                    } else {
                        stats.nodes_from_relations += 1;
                    }
                }
            }
            Element::Way(w) => {
                if ids.included_way_ids.get(w.id()) {
                    ensure_way_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                    if ids.direct_way_ids.get(w.id()) {
                        stats.ways_matched += 1;
                    } else {
                        stats.ways_from_relations += 1;
                    }
                }
            }
            Element::Relation(r) => {
                if ids.included_relation_ids.get(r.id()) {
                    ensure_relation_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                    if ids.direct_relation_ids.get(r.id()) {
                        stats.relations_matched += 1;
                    } else {
                        stats.relations_from_relations += 1;
                    }
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
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, TagsFilterStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = filter_block_pass2(block, ids, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    drain_batch_results(results, writer, |s| {
        stats.nodes_matched += s.nodes_matched;
        stats.nodes_from_ways += s.nodes_from_ways;
        stats.nodes_from_relations += s.nodes_from_relations;
        stats.ways_matched += s.ways_matched;
        stats.ways_from_relations += s.ways_from_relations;
        stats.relations_matched += s.relations_matched;
        stats.relations_from_relations += s.relations_from_relations;
    })?;
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
        nodes_from_relations: 0,
        ways_matched: 0,
        ways_from_relations: 0,
        relations_matched: 0,
        relations_from_relations: 0,
    };

    // --- Pass 1: Collect match results and way dependencies ---
    //
    // IdSetDense: O(1) set/get, 1 bit per ID, ~1.5 GB max for planet-scale node IDs.
    // No sort/dedup step needed between passes (bitset deduplicates inherently).
    let mut matched_node_ids = IdSetDense::new();
    let mut direct_way_ids = IdSetDense::new();
    let mut included_way_ids = IdSetDense::new();
    let mut direct_relation_ids = IdSetDense::new();
    let mut included_relation_ids = IdSetDense::new();
    let mut way_dep_node_ids = IdSetDense::new();
    let mut relation_dep_node_ids = IdSetDense::new();
    let mut has_included_way = false;
    let mut has_included_relation = false;

    // Pass 1: skip blob types that no expression targets.
    let reader = ElementReader::open(input, direct_io)?;
    let reader = match blob_filter_from_expressions(expressions) {
        Some(filter) => reader.with_blob_filter(filter),
        None => reader,
    };
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        let mut tags_buf: Vec<(&str, &str)> = Vec::new();
        for element in block.elements_skip_metadata() {
            match &element {
                Element::DenseNode(dn) => {
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    if element_matches(expressions, &tags_buf, true, false, false) {
                        matched_node_ids.set(dn.id());
                    }
                }
                Element::Node(n) => {
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    if element_matches(expressions, &tags_buf, true, false, false) {
                        matched_node_ids.set(n.id());
                    }
                }
                Element::Way(w) => {
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    if element_matches(expressions, &tags_buf, false, true, false) {
                        direct_way_ids.set(w.id());
                        if set_if_absent(&mut included_way_ids, w.id()) {
                            has_included_way = true;
                        }
                        for r in w.refs() {
                            way_dep_node_ids.set(r);
                        }
                    }
                }
                Element::Relation(r) => {
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
                    if element_matches(expressions, &tags_buf, false, false, true) {
                        direct_relation_ids.set(r.id());
                        if set_if_absent(&mut included_relation_ids, r.id()) {
                            has_included_relation = true;
                        }
                    }
                }
            }
        }
    }

    // Expand relation-member closure:
    // - matched relation -> include member nodes/ways/relations
    // - member relations recurse transitively (cycle-safe via set membership)
    let closure = collect_relation_member_closure(
        input,
        direct_io,
        &mut included_relation_ids,
        &mut included_way_ids,
        &mut relation_dep_node_ids,
    )?;
    has_included_way |= closure.has_way;
    has_included_relation |= closure.has_relation;

    // Any included way (direct match or pulled from relation members) contributes node deps.
    collect_way_node_dependencies(
        input,
        direct_io,
        &included_way_ids,
        Some(&direct_way_ids),
        &mut relation_dep_node_ids,
    )?;

    // --- Pass 2: Write matching elements in file order (parallel batches) ---
    // Pass 2 always needs nodes (for way deps) plus matched ways/relations.
    let reader = ElementReader::open(input, direct_io)?;
    let reader = if blob_filter_from_expressions(expressions).is_some() {
        // Nodes are always needed for dependency expansion.
        // Ways/relations are included when either directly matched or pulled via relation closure.
        reader.with_blob_filter(BlobFilter::new(true, has_included_way, has_included_relation))
    } else {
        reader
    };
    let mut writer = writer_from_header(output, compression, reader.header(), true, |hb| hb)?;

    let id_sets = Pass2IdSets {
        matched_node_ids: &matched_node_ids,
        direct_way_ids: &direct_way_ids,
        included_way_ids: &included_way_ids,
        direct_relation_ids: &direct_relation_ids,
        included_relation_ids: &included_relation_ids,
        way_dep_node_ids: &way_dep_node_ids,
        relation_dep_node_ids: &relation_dep_node_ids,
    };

    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        process_pass2_batch(batch, &id_sets, &mut writer, &mut stats)
    })?;

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Set an ID in the dense set and return whether it was newly inserted.
fn set_if_absent(set: &mut IdSetDense, id: i64) -> bool {
    if set.get(id) {
        return false;
    }
    set.set(id);
    true
}

#[derive(Clone, Copy, Debug, Default)]
struct RelationClosureSummary {
    has_way: bool,
    has_relation: bool,
}

/// Expand relation membership transitively for default tags-filter mode.
///
/// Starts from already-included relation IDs and repeatedly scans relation blobs:
/// included relation -> include member nodes/ways/relations.
/// Recursion terminates when no new relation IDs are discovered.
fn collect_relation_member_closure(
    input: &Path,
    direct_io: bool,
    included_relation_ids: &mut IdSetDense,
    included_way_ids: &mut IdSetDense,
    relation_dep_node_ids: &mut IdSetDense,
) -> Result<RelationClosureSummary> {
    let mut summary = RelationClosureSummary::default();

    loop {
        let mut added_relations = 0_u64;
        let reader = ElementReader::open(input, direct_io)?
            .with_blob_filter(BlobFilter::only_relations());

        for block in reader.into_blocks_pipelined() {
            let block = block?;
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element {
                    if !included_relation_ids.get(r.id()) {
                        continue;
                    }
                    summary.has_relation = true;
                    for member in r.members() {
                        match member.id {
                            MemberId::Node(id) => {
                                relation_dep_node_ids.set(id);
                            }
                            MemberId::Way(id) => {
                                if set_if_absent(included_way_ids, id) {
                                    summary.has_way = true;
                                }
                            }
                            MemberId::Relation(id) => {
                                if set_if_absent(included_relation_ids, id) {
                                    added_relations += 1;
                                }
                            }
                            MemberId::Unknown(..) => {}
                        }
                    }
                }
            }
        }

        if added_relations == 0 {
            break;
        }
    }

    Ok(summary)
}

/// Scan all way blobs and add node refs for ways selected for output.
fn collect_way_node_dependencies(
    input: &Path,
    direct_io: bool,
    included_way_ids: &IdSetDense,
    skip_way_ids: Option<&IdSetDense>,
    relation_dep_node_ids: &mut IdSetDense,
) -> Result<()> {
    let reader = ElementReader::open(input, direct_io)?
        .with_blob_filter(BlobFilter::new(false, true, false));
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        for element in block.elements_skip_metadata() {
            if let Element::Way(w) = &element
                && included_way_ids.get(w.id())
            {
                if let Some(skip) = skip_way_ids
                    && skip.get(w.id())
                {
                    continue;
                }
                for node_id in w.refs() {
                    relation_dep_node_ids.set(node_id);
                }
            }
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
