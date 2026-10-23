//! Filter elements by tag expressions. Equivalent to `osmium tags-filter`.

pub mod osc;

use std::path::Path;

use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, require_indexdata, writer_from_header,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::idset::IdSet;
use crate::owned::{dense_node_metadata, element_metadata};
use crate::tag_expr::{Expression, StringMatcher, parse_expressions, tag_matches};
use crate::write::WriteOptions;
use crate::{BlobFilter, Element, ElementReader, MemberId, PrimitiveBlock};

use super::Result;

/// Compute a `BlobFilter` from the union of all expression type + tag key filters.
///
/// Returns `None` only if all types are needed AND no tag keys can be extracted
/// (no benefit from filtering at blob level).
///
/// # Key matchers that constrain nothing
///
/// The blob-level filter can only express "this blob's tag index contains one
/// of these exact keys, or a key under one of these prefixes". A
/// [`StringMatcher::Substring`] key (`*:name`) or a
/// [`StringMatcher::Always`] key (`*`) fits neither, and there is no
/// conservative approximation of a substring in a key-set membership test. An
/// expression carrying one therefore disables tag-level blob filtering
/// **entirely** - keys and prefixes contributed by the *other* expressions
/// included, since the filter's semantics are a union and skipping a blob on
/// the union would skip blobs the unconstrained expression could still match.
///
/// This is the layer above the matcher rewrite, and the place it could have
/// gone wrong: widening the matcher without widening what the blob filter
/// refuses to constrain would have turned newly-valid expressions into
/// silently-skipped blobs, which is exactly the class of bug the rewrite was
/// closing.
pub(crate) fn blob_filter_from_expressions(expressions: &[Expression]) -> Option<BlobFilter> {
    let mut need_nodes = false;
    let mut need_ways = false;
    let mut need_relations = false;
    let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut prefixes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut key_unconstrained = false;

    for expr in expressions {
        need_nodes |= expr.type_filter.nodes;
        need_ways |= expr.type_filter.ways;
        need_relations |= expr.type_filter.relations;

        match &expr.matcher.key {
            StringMatcher::Equal(key) => {
                keys.insert(key.clone());
            }
            StringMatcher::List(alternatives) => {
                keys.extend(alternatives.iter().cloned());
            }
            StringMatcher::Prefix(prefix) => {
                prefixes.insert(prefix.clone());
            }
            StringMatcher::Substring(_) | StringMatcher::Always => {
                key_unconstrained = true;
            }
        }
    }

    if key_unconstrained {
        keys.clear();
        prefixes.clear();
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

/// The blob filter to run with, or `None` to decode every blob.
///
/// Two reasons to decline blob-level filtering: `invert` (we want the blobs
/// the filter would reject), and `distrust_metadata` (the caller has said the
/// file's own claims about its blobs are not to be believed - see
/// [`TagsFilterOptions::distrust_metadata`]).
fn trusted_blob_filter(
    expressions: &[Expression],
    invert: bool,
    distrust_metadata: bool,
) -> Option<BlobFilter> {
    if invert || distrust_metadata {
        return None;
    }
    blob_filter_from_expressions(expressions)
}

/// Check if an element's tags match any applicable expression (OR semantics).
///
/// Callers pass exactly one kind. Keep the expression-first traversal: it
/// rejects an inapplicable kind before walking tags. Unlike tags-count this
/// selects the whole element, and unlike export an empty list matches nothing.
/// The shared `tag_matches` owns wildcard and negation semantics; this loop
/// only supplies the element quantifier on the measured classify/write paths.
fn element_matches(
    expressions: &[Expression],
    tags: &[(&str, &str)],
    kind: crate::ElementKind,
) -> bool {
    for expr in expressions {
        if !expr.type_filter.admits(kind) {
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
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self.nodes_matched
            + self.nodes_from_ways
            + self.nodes_from_relations
            + self.ways_matched
            + self.ways_from_relations
            + self.relations_matched
            + self.relations_from_relations;
        writeln!(
            w,
            "Wrote {total} elements: {} nodes ({} direct + {} from ways + {} from relations), \
             {} ways ({} direct + {} from relations), {} relations ({} direct + {} from relations)",
            self.nodes_matched + self.nodes_from_ways + self.nodes_from_relations,
            self.nodes_matched,
            self.nodes_from_ways,
            self.nodes_from_relations,
            self.ways_matched + self.ways_from_relations,
            self.ways_matched,
            self.ways_from_relations,
            self.relations_matched + self.relations_from_relations,
            self.relations_matched,
            self.relations_from_relations,
        )?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Options for the tags-filter command.
pub struct TagsFilterOptions<'a> {
    pub expression_strs: &'a [String],
    pub omit_referenced: bool,
    pub invert: bool,
    /// Strip tags from referenced objects not directly matched by expressions.
    /// Only meaningful without `-R` (two-pass mode with reference expansion).
    pub remove_tags: bool,
    /// Output compression, backend and the shared input flags.
    pub write: WriteOptions,
    /// Override the worker-pool size for the parallel classify passes
    /// in two-pass mode (`parallel_classify_phase` for blob scanning
    /// plus two `parallel_classify_accumulate` calls for relation
    /// closure + way-node deps). `None` uses the rayon default
    /// (`available_parallelism()`). Ignored in single-pass mode
    /// (`-R`, which uses the pipelined reader rather than
    /// `parallel_classify_*`); the CLI rejects that combination
    /// upfront.
    pub jobs: Option<usize>,
    /// Do not trust the per-blob sidecar metadata. Every blob is decompressed
    /// and every element examined, regardless of what the blob's `indexdata`
    /// and tag index claim about its contents.
    ///
    /// Blob-level skipping on the tag index is a trust boundary with nothing
    /// on the other side of it: no code path validates that a blob's tag index
    /// matches its payload, so a file whose tag index omits a key its elements
    /// actually carry silently loses those elements from the output. That is a
    /// *wrong answer*, not a slow one, and it is invisible - the skipped blob
    /// was never decoded, so nothing can notice. This flag is the escape
    /// hatch: it costs the blob-skip speedup and buys an answer that depends
    /// only on element data.
    pub distrust_metadata: bool,
}

/// Filter a PBF file by tag expressions.
///
/// If `omit_referenced` is true (`-R` flag), only directly matching elements
/// are output (single pass, faster). Otherwise, referenced nodes of matching
/// ways are also included (two-pass).
pub fn tags_filter(
    input: &Path,
    output: crate::output::Output,
    opts: &TagsFilterOptions<'_>,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<TagsFilterStats> {
    let result = tags_filter_to(input, &output, opts, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`tags_filter`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn tags_filter_to(
    input: &Path,
    output: &crate::output::Output,
    opts: &TagsFilterOptions<'_>,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<TagsFilterStats> {
    // Blob-level filtering can't help in invert mode (we want non-matching
    // blobs), and `--distrust-metadata` declines to use it at all - demanding
    // indexdata the run will not consult would be a pointless refusal.
    if !opts.invert && !opts.distrust_metadata {
        require_indexdata(
            input,
            opts.write.direct_reads,
            opts.write.force,
            "input PBF has no blob-level indexdata. Without indexdata, type and tag key \
             filters are no-ops - all blobs are decompressed (significantly slower).",
            host,
        )?;
    }

    if let Some(n) = opts.jobs {
        #[allow(clippy::cast_possible_wrap)]
        crate::debug::emit_counter("tagsfilter_worker_count", n as i64);
    }

    let expressions = parse_expressions(opts.expression_strs)?;
    if opts.omit_referenced {
        let result = tags_filter_single_pass(
            input,
            output,
            &expressions,
            opts.invert,
            opts.distrust_metadata,
            &opts.write,
            overrides,
            host,
        )?;
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("tagsfilter_matched_nodes", result.nodes_matched as i64);
            crate::debug::emit_counter("tagsfilter_matched_ways", result.ways_matched as i64);
            crate::debug::emit_counter(
                "tagsfilter_matched_relations",
                result.relations_matched as i64,
            );
        }
        Ok(result)
    } else {
        let result = tags_filter_two_pass(
            input,
            output,
            &expressions,
            opts.invert,
            opts.distrust_metadata,
            opts.remove_tags,
            &opts.write,
            opts.jobs,
            overrides,
            host,
        )?;
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("tagsfilter_matched_nodes", result.nodes_matched as i64);
            crate::debug::emit_counter("tagsfilter_matched_ways", result.ways_matched as i64);
            crate::debug::emit_counter(
                "tagsfilter_matched_relations",
                result.relations_matched as i64,
            );
            crate::debug::emit_counter("tagsfilter_nodes_from_ways", result.nodes_from_ways as i64);
            crate::debug::emit_counter(
                "tagsfilter_nodes_from_relations",
                result.nodes_from_relations as i64,
            );
            crate::debug::emit_counter(
                "tagsfilter_ways_from_relations",
                result.ways_from_relations as i64,
            );
            crate::debug::emit_counter(
                "tagsfilter_relations_from_relations",
                result.relations_from_relations as i64,
            );
        }
        Ok(result)
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
    invert: bool,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<TagsFilterStats> {
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
                if element_matches(expressions, &tags_buf, crate::ElementKind::Node) != invert {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = dense_node_metadata(dn);
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        tags_buf.iter().copied(),
                        meta.as_ref(),
                    );
                    stats.nodes_matched += 1;
                }
            }
            Element::Node(n) => {
                tags_buf.clear();
                tags_buf.extend(n.tags());
                if element_matches(expressions, &tags_buf, crate::ElementKind::Node) != invert {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = element_metadata(&n.info());
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        tags_buf.iter().copied(),
                        meta.as_ref(),
                    );
                    stats.nodes_matched += 1;
                }
            }
            Element::Way(w) => {
                tags_buf.clear();
                tags_buf.extend(w.tags());
                if element_matches(expressions, &tags_buf, crate::ElementKind::Way) != invert {
                    ensure_way_capacity_local(bb, output)?;
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), tags_buf.iter().copied(), &refs_buf, meta.as_ref());
                    stats.ways_matched += 1;
                }
            }
            Element::Relation(r) => {
                tags_buf.clear();
                tags_buf.extend(r.tags());
                if element_matches(expressions, &tags_buf, crate::ElementKind::Relation) != invert {
                    ensure_relation_capacity_local(bb, output)?;
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(
                        r.id(),
                        tags_buf.iter().copied(),
                        &members_buf,
                        meta.as_ref(),
                    );
                    stats.relations_matched += 1;
                }
            }
        }
    }
    Ok(stats)
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn tags_filter_single_pass(
    input: &Path,
    output: &crate::output::Output,
    expressions: &[Expression],
    invert: bool,
    distrust_metadata: bool,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<TagsFilterStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        ..
    } = *write;
    crate::debug::emit_marker("TAGSFILTER_SCAN_START");
    let reader = ElementReader::open_in(input, direct_io, host)?;
    // Blob-level filtering can't help in invert mode - we want non-matching blobs.
    let reader = match trusted_blob_filter(expressions, invert, distrust_metadata) {
        Some(filter) => reader.with_blob_filter(filter),
        None => reader,
    };
    let mut writer = writer_from_header(
        output,
        compression,
        reader.header(),
        &super::output_header::OutputFacts::carry(super::output_header::Ways::Reencoded),
        overrides,
        backend,
        host,
    )?;
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_matched: 0,
        ways_from_relations: 0,
        relations_matched: 0,
        relations_from_relations: 0,
    };

    reader.for_each_fused_block(
        |block| {
            let mut bb = BlockBuilder::new();
            let mut output = Vec::new();
            let block_stats =
                filter_block_parallel(&block, expressions, invert, &mut bb, &mut output)?;
            flush_local(&mut bb, &mut output)?;
            Ok((output, block_stats))
        },
        |(blocks, block_stats)| {
            for OwnedBlock {
                bytes,
                index,
                tagdata,
                way_members,
            } in blocks
            {
                writer.write_primitive_block_owned(
                    bytes,
                    index,
                    tagdata.as_deref(),
                    way_members.as_deref(),
                )?;
            }
            stats.nodes_matched += block_stats.nodes_matched;
            stats.ways_matched += block_stats.ways_matched;
            stats.relations_matched += block_stats.relations_matched;
            Ok(())
        },
    )?;

    writer.flush()?;
    crate::debug::emit_marker("TAGSFILTER_SCAN_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Two-pass filter: Pass 2 parallel helpers
// ---------------------------------------------------------------------------

/// Read-only ID sets for Pass 2, shared across rayon threads.
struct Pass2IdSets<'a> {
    matched_node_ids: &'a IdSet,
    direct_way_ids: &'a IdSet,
    included_way_ids: &'a IdSet,
    direct_relation_ids: &'a IdSet,
    included_relation_ids: &'a IdSet,
    way_dep_node_ids: &'a IdSet,
    relation_dep_node_ids: &'a IdSet,
}

/// Process a single block in Pass 2: write elements whose IDs were collected in Pass 1.
#[allow(clippy::too_many_lines)]
fn filter_block_pass2(
    block: &PrimitiveBlock,
    ids: &Pass2IdSets<'_>,
    remove_tags: bool,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<TagsFilterStats> {
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
                    if !remove_tags || direct {
                        tags_buf.extend(dn.tags());
                    }
                    let meta = dense_node_metadata(dn);
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        tags_buf.iter().copied(),
                        meta.as_ref(),
                    );
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
                    if !remove_tags || direct {
                        tags_buf.extend(n.tags());
                    }
                    let meta = element_metadata(&n.info());
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        tags_buf.iter().copied(),
                        meta.as_ref(),
                    );
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
                    let direct = ids.direct_way_ids.get(w.id());
                    if !remove_tags || direct {
                        tags_buf.extend(w.tags());
                    }
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), tags_buf.iter().copied(), &refs_buf, meta.as_ref());
                    if direct {
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
                    let direct = ids.direct_relation_ids.get(r.id());
                    if !remove_tags || direct {
                        tags_buf.extend(r.tags());
                    }
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(
                        r.id(),
                        tags_buf.iter().copied(),
                        &members_buf,
                        meta.as_ref(),
                    );
                    if direct {
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

// ---------------------------------------------------------------------------
// Two-pass filter (default mode, include references)
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn tags_filter_two_pass(
    input: &Path,
    output: &crate::output::Output,
    expressions: &[Expression],
    invert: bool,
    distrust_metadata: bool,
    remove_tags: bool,
    write: &WriteOptions,
    jobs: Option<usize>,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<TagsFilterStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        ..
    } = *write;
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
    crate::debug::emit_marker("TAGSFILTER_PASS1_START");
    //
    // IdSet: O(1) set/get, 1 bit per ID, ~1.5 GB max for planet-scale node IDs.
    // No sort/dedup step needed between passes (bitset deduplicates inherently).
    let mut matched_node_ids = IdSet::new();
    let mut direct_way_ids = IdSet::new();
    let mut included_way_ids = IdSet::new();
    let mut direct_relation_ids = IdSet::new();
    let mut included_relation_ids = IdSet::new();
    let mut way_dep_node_ids = IdSet::new();
    let mut relation_dep_node_ids = IdSet::new();
    let mut has_included_way = false;
    let mut has_included_relation = false;

    // Pass 1: parallel classification via pread-from-workers.
    // Workers own the full PrimitiveBlock lifecycle (pread → decompress →
    // PrimitiveBlock → tag match → send compact results). Consumer merges
    // matching IDs into the IdSet collections.
    {
        // Refuse a file without a readable header before pass 1. (The output
        // header, and its warning about undeclared enrichment, is built
        // before pass 2.)
        let mut header_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
        header_reader.set_parse_indexdata(true);
        let header_blob = header_reader.leading_header()?;
        header_blob.to_headerblock()?;
        drop(header_reader);

        // Build schedule from header-only scan via the pread-only HeaderWalker.
        // Avoids pulling blob bodies into the page cache for blobs we'll decide
        // to skip on the tag-index filter anyway.
        crate::debug::emit_marker("TAGSFILTER_SINGLE_PASS_SCHEDULE_SCAN_START");
        let mut walker = crate::read::header_walker::HeaderWalker::open(input, &host.diagnostics)?;
        let _ = walker
            .next_header()?
            .ok_or_else(|| crate::error::new_missing_header(input))?;

        let expr_filter = trusted_blob_filter(expressions, invert, distrust_metadata);

        let mut schedule: Vec<(usize, u64, usize)> = Vec::new();
        let mut blobs_skipped_by_tag: u64 = 0;
        let mut seq: usize = 0;
        while let Some(meta) = walker.next_header()? {
            if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
                continue;
            }
            // Same predicate as `build_classify_schedule_blob_filtered`: only
            // a kind or bbox filter loses pruning on an unindexed blob.
            if expr_filter.as_ref().is_some_and(BlobFilter::uses_index) {
                walker.observe_filter_index(meta.index.as_ref());
            }
            let (skip, tag_skip) = crate::scan::classify::blob_filter_check(
                meta.index.as_ref(),
                meta.tagdata.as_deref(),
                expr_filter.as_ref(),
            );
            if skip {
                if tag_skip {
                    blobs_skipped_by_tag += 1;
                }
                continue;
            }
            schedule.push((seq, meta.data_offset, meta.data_size));
            seq += 1;
        }
        if blobs_skipped_by_tag > 0 {
            crate::host::diag!(
                host.diagnostics,
                Note,
                "tags_filter.blobs_skipped_by_tag",
                "{blobs_skipped_by_tag} blobs skipped by tag index"
            );
        }
        crate::debug::emit_marker("TAGSFILTER_SINGLE_PASS_SCHEDULE_SCAN_END");

        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter(
                "tagsfilter_single_pass_schedule_blobs",
                schedule.len() as i64,
            );
            crate::debug::emit_counter(
                "tagsfilter_single_pass_tagidx_skipped_blobs",
                blobs_skipped_by_tag as i64,
            );
        }

        let shared_file = std::sync::Arc::clone(walker.shared_file());
        drop(walker);

        /// Per-blob classification result from a worker.
        struct ClassifyResult {
            matched_nodes: Vec<i64>,
            matched_ways: Vec<(i64, Vec<i64>)>, // (way_id, refs)
            matched_relations: Vec<i64>,
        }

        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &schedule,
            jobs,
            || (),
            |block, _s| {
                let mut result = ClassifyResult {
                    matched_nodes: Vec::new(),
                    matched_ways: Vec::new(),
                    matched_relations: Vec::new(),
                };
                let mut tags_buf: Vec<(&str, &str)> = Vec::new();
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn) => {
                            tags_buf.clear();
                            tags_buf.extend(dn.tags());
                            if element_matches(expressions, &tags_buf, crate::ElementKind::Node)
                                != invert
                            {
                                result.matched_nodes.push(dn.id());
                            }
                        }
                        Element::Node(n) => {
                            tags_buf.clear();
                            tags_buf.extend(n.tags());
                            if element_matches(expressions, &tags_buf, crate::ElementKind::Node)
                                != invert
                            {
                                result.matched_nodes.push(n.id());
                            }
                        }
                        Element::Way(w) => {
                            tags_buf.clear();
                            tags_buf.extend(w.tags());
                            if element_matches(expressions, &tags_buf, crate::ElementKind::Way)
                                != invert
                            {
                                let refs: Vec<i64> = w.refs().collect();
                                result.matched_ways.push((w.id(), refs));
                            }
                        }
                        Element::Relation(r) => {
                            tags_buf.clear();
                            tags_buf.extend(r.tags());
                            if element_matches(expressions, &tags_buf, crate::ElementKind::Relation)
                                != invert
                            {
                                result.matched_relations.push(r.id());
                            }
                        }
                    }
                }
                result
            },
            |_seq, cr| {
                for id in cr.matched_nodes {
                    matched_node_ids.set(id);
                }
                for (way_id, refs) in cr.matched_ways {
                    direct_way_ids.set(way_id);
                    if included_way_ids.set_if_new(way_id) {
                        has_included_way = true;
                    }
                    for r in refs {
                        way_dep_node_ids.set(r);
                    }
                }
                for id in cr.matched_relations {
                    direct_relation_ids.set(id);
                    if included_relation_ids.set_if_new(id) {
                        has_included_relation = true;
                    }
                }
            },
            &*host.resources,
        )?;
    }

    crate::debug::emit_marker("TAGSFILTER_PASS1_END");

    // Expand relation-member closure (skip if no relations matched):
    // - matched relation -> include member nodes/ways/relations
    // - member relations recurse transitively (cycle-safe via set membership)
    crate::debug::emit_marker("TAGSFILTER_CLOSURE_START");
    if has_included_relation {
        let closure = collect_relation_member_closure(
            input,
            direct_io,
            distrust_metadata,
            &mut included_relation_ids,
            &mut included_way_ids,
            &mut relation_dep_node_ids,
            jobs,
            host,
        )?;
        has_included_way |= closure.has_way;
        has_included_relation |= closure.has_relation;
    }
    crate::debug::emit_marker("TAGSFILTER_CLOSURE_END");

    // Any included way (direct match or pulled from relation members) contributes node deps.
    crate::debug::emit_marker("TAGSFILTER_WAYDEPS_START");
    collect_way_node_dependencies(
        input,
        direct_io,
        distrust_metadata,
        &included_way_ids,
        Some(&direct_way_ids),
        &mut relation_dep_node_ids,
        jobs,
        host,
    )?;
    crate::debug::emit_marker("TAGSFILTER_WAYDEPS_END");

    // --- Pass 2: Write matching elements via pread-from-workers ---
    // Way/relation blobs where ALL elements are included pass through as raw
    // frames (zero decompression + re-encoding). Remaining blobs are decoded
    // and filtered by parallel workers.
    crate::debug::emit_marker("TAGSFILTER_PASS2_START");

    let mut header_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
    let header_blob = header_reader.leading_header()?;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_header::OutputFacts::carry(super::output_header::Ways::Reencoded),
        overrides,
        backend,
        host,
    )?;

    // Build pass 2 schedule. Skip blob types not needed (type filter only -
    // no tag index filtering because elements can be included via relation
    // closure without having the matching tag key).
    // `distrust_metadata` drops this filter too: its element-kind decision
    // comes from each blob's indexdata, so a blob whose indexdata mislabels
    // its kind would be skipped with its contents unexamined.
    let blob_filter: Option<BlobFilter> = if invert || distrust_metadata {
        None
    } else {
        Some(BlobFilter::new(
            true,
            has_included_way,
            has_included_relation,
        ))
    };

    crate::debug::emit_marker("TAGSFILTER_PASS2_SCHEDULE_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, &host.diagnostics)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input))?;

    let mut schedule: Vec<(u64, usize)> = Vec::new(); // (data_offset, data_size)
    let mut blobs_skipped: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }

        // Type-only filter: skip blob types not needed (no tag index filtering
        // in pass 2 - elements can be included via relation closure without
        // having the matching tag key).
        //
        // The `if let Some(idx) = meta.index.as_ref()` gate means non-indexed
        // blobs fall through unskipped and decode fully in pass 2. This is
        // only reachable when the upstream `require_indexdata` at :177 was
        // waived by `opts.force` or `opts.invert` - both explicit slow-path
        // opt-ins where the user accepts full decode. If that upstream gate
        // is ever relaxed, this site needs tightening: either bail out
        // earlier for non-indexed blobs, or decode-to-check the blob's kind
        // before admitting it to the schedule.
        if blob_filter.as_ref().is_some_and(BlobFilter::uses_index) {
            walker.observe_filter_index(meta.index.as_ref());
        }
        if let Some(ref filter) = blob_filter
            && let Some(idx) = meta.index.as_ref()
            && !filter.wants_index(idx)
        {
            blobs_skipped += 1;
            continue;
        }

        schedule.push((meta.data_offset, meta.data_size));
    }
    drop(walker);
    crate::debug::emit_marker("TAGSFILTER_PASS2_SCHEDULE_SCAN_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("tagsfilter_pass2_schedule_blobs", schedule.len() as i64);
        crate::debug::emit_counter("tagsfilter_pass2_skipped_blobs", blobs_skipped as i64);
    }

    if blobs_skipped > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "tags_filter.pass2_blobs_skipped",
            "pass 2: {blobs_skipped} blobs skipped by type/tag index"
        );
    }

    let id_sets = Pass2IdSets {
        matched_node_ids: &matched_node_ids,
        direct_way_ids: &direct_way_ids,
        included_way_ids: &included_way_ids,
        direct_relation_ids: &direct_relation_ids,
        included_relation_ids: &included_relation_ids,
        way_dep_node_ids: &way_dep_node_ids,
        relation_dep_node_ids: &relation_dep_node_ids,
    };

    // pread-from-workers: parallel decode + filter + write with reorder buffer.
    //
    // DO NOT add blob-level raw passthrough here (wire-format ID scanner,
    // "all elements in this blob are in the include set -> write blob raw").
    //
    // Measured end-to-end on 2026-04-18 via a shadow counter that did the
    // full ID-set classification per blob without actually passing anything
    // through raw. Commit `a5c6854` (shadow added) reverted in `0ef4107`
    // (shadow removed), UUID `8c786794`, `w/highway=primary` on planet:
    //
    //   0 / 50,364 pass-2 blobs would have qualified. Zero. Across
    //   17,529 way blobs (0.34 % per-element match rate) and 32,835
    //   node blobs (0.40 % per-element match rate).
    //
    // The math is hostile in general, not just for highway=primary:
    // ~8,000 elements per blob, any realistic per-element match rate
    // (highway=primary at 0.34 %, building=* hypothetically ~10 %),
    // P(all elements match in a blob) is vanishingly small. PBFs are
    // sorted by ID rather than by geography or tag, so matching elements
    // are scattered across every blob rather than clustered into a few.
    // A filter that did match every element would already be caught by
    // blob-level type/tag-index filtering upstream of this code.
    //
    // The stricter `all_direct` gate (required when `remove_tags` is set,
    // since raw passthrough cannot strip tags off non-direct matches) is
    // tighter than `all_included` and also measured 0 at planet.
    //
    // So: no ID scanner here. No per-group scanner either. This comment
    // is the pin that keeps the door shut. Pread workers are kept for
    // planet safety (no cross-thread PrimitiveBlock retention), not for
    // passthrough.
    {
        use std::os::unix::fs::FileExt as _;

        let shared_file = std::sync::Arc::new(
            std::fs::File::open(input).map_err(|e| crate::file_reader::open_error(input, e))?,
        );

        let decode_threads = crate::host::sized_workers(
            &*host.resources,
            crate::tunables::DECODE_WORKERS_RESERVED,
            crate::tunables::DECODE_WORKERS_MIN,
            crate::tunables::DECODE_WORKERS_FALLBACK,
        );

        let decode_items: Vec<(usize, u64, usize)> = schedule
            .iter()
            .enumerate()
            .map(|(i, &(data_offset, data_size))| (i, data_offset, data_size))
            .collect();
        let mut willneed = crate::scan::classify::WillneedPrefetch::new(&decode_items);

        type WorkerResult = (
            usize,
            crate::error::Result<(Vec<OwnedBlock>, TagsFilterStats)>,
        );
        let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(
            crate::tunables::POOL_DESCRIPTOR_CHANNEL_DEPTH,
        );
        let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<WorkerResult>(
            crate::tunables::POOL_RESULT_CHANNEL_DEPTH,
        );
        let prefetch_file = std::sync::Arc::clone(&shared_file);

        std::thread::scope(|scope| -> Result<()> {
            scope.spawn(move || {
                for item in decode_items {
                    willneed.before_dispatch(&prefetch_file, item);
                    if desc_tx.send(item).is_err() {
                        break;
                    }
                }
            });

            for _ in 0..decode_threads {
                let rx = std::sync::Arc::clone(&desc_rx);
                let tx = result_tx.clone();
                let file = std::sync::Arc::clone(&shared_file);
                let ids_ref = &id_sets;
                scope.spawn(move || {
                    let mut read_buf: Vec<u8> = Vec::new();
                    let worker_pool = crate::blob::DecompressPool::new();
                    let mut bb = BlockBuilder::new();
                    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
                    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

                    loop {
                        let (s, data_offset, data_size) = {
                            let guard =
                                rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => break,
                            }
                        };

                        let r: crate::error::Result<(Vec<OwnedBlock>, TagsFilterStats)> = (|| {
                            read_buf.resize(data_size, 0);
                            file.read_exact_at(&mut read_buf, data_offset)?;
                            let mut buf = crate::blob::pool_get(
                                Some(&worker_pool),
                                data_size * crate::tunables::DECOMPRESS_EXPANSION_GUESS,
                            );
                            crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                            let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                                buf,
                                &worker_pool,
                                &mut st_scratch,
                                &mut gr_scratch,
                            )?;
                            output_blocks.clear();
                            let block_stats = filter_block_pass2(
                                &block,
                                ids_ref,
                                remove_tags,
                                &mut bb,
                                &mut output_blocks,
                            )?;
                            flush_local(&mut bb, &mut output_blocks)?;
                            Ok((std::mem::take(&mut output_blocks), block_stats))
                        })(
                        );
                        if tx.send((s, r)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(desc_rx);
            drop(result_tx);

            // Consumer: merge results via reorder buffer for file-order output.
            let mut reorder: crate::reorder_buffer::ReorderBuffer<
                crate::error::Result<(Vec<OwnedBlock>, TagsFilterStats)>,
            > = crate::reorder_buffer::ReorderBuffer::with_capacity(
                crate::tunables::POOL_REORDER_CAPACITY,
            );

            let mut drain_ready =
                |reorder: &mut crate::reorder_buffer::ReorderBuffer<_>| -> Result<()> {
                    while let Some(r) = reorder.pop_ready() {
                        let (blocks, block_stats): (Vec<OwnedBlock>, TagsFilterStats) = r?;
                        stats.nodes_matched += block_stats.nodes_matched;
                        stats.nodes_from_ways += block_stats.nodes_from_ways;
                        stats.nodes_from_relations += block_stats.nodes_from_relations;
                        stats.ways_matched += block_stats.ways_matched;
                        stats.ways_from_relations += block_stats.ways_from_relations;
                        stats.relations_matched += block_stats.relations_matched;
                        stats.relations_from_relations += block_stats.relations_from_relations;
                        for OwnedBlock {
                            bytes: block_bytes,
                            index,
                            tagdata,
                            way_members,
                        } in blocks
                        {
                            writer.write_primitive_block_owned(
                                block_bytes,
                                index,
                                tagdata.as_deref(),
                                way_members.as_deref(),
                            )?;
                        }
                    }
                    Ok(())
                };

            for (s, item) in result_rx {
                reorder.push(s, item);
                drain_ready(&mut reorder)?;
            }
            drain_ready(&mut reorder)?;

            Ok(())
        })?;
    }

    writer.flush()?;
    crate::debug::emit_marker("TAGSFILTER_PASS2_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
///
/// `distrust_metadata` widens the schedule to every OsmData blob. See
/// [`kind_schedule_filter`].
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn collect_relation_member_closure(
    input: &Path,
    _direct_io: bool,
    distrust_metadata: bool,
    included_relation_ids: &mut IdSet,
    included_way_ids: &mut IdSet,
    relation_dep_node_ids: &mut IdSet,
    jobs: Option<usize>,
    host: &crate::host::Host,
) -> Result<RelationClosureSummary> {
    let mut summary = RelationClosureSummary::default();

    // Build schedule once - reused across convergence iterations.
    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        kind_schedule_filter(crate::ElementKind::Relation, distrust_metadata),
        &host.diagnostics,
    )?;

    struct ClosureResult {
        node_ids: Vec<i64>,
        way_ids: Vec<i64>,
        relation_ids: Vec<i64>,
    }

    loop {
        let mut added_relations = 0_u64;

        // Classify phase: workers read included_relation_ids (immutable).
        // Results collected into a Vec - merge phase runs after with mutable access.
        // Note: this uses parallel_classify_accumulate (per-worker
        // accumulation) rather than parallel_classify_phase. The merge
        // step needs `&mut included_relation_ids` while the classify
        // step needs `&included_relation_ids`, so they cannot co-exist
        // in the same parallel_classify_phase call. Per-worker
        // accumulation is fine here at planet (per-worker Vec<i64>
        // tops at ~80 MB even in the worst invert-match case;
        // measured 6.8 GB total for this phase). collect_way_node_deps
        // is the one that hit 24 GB and needed the per-blob shape.
        let mut results: Vec<ClosureResult> = Vec::new();
        crate::scan::classify::parallel_classify_accumulate(
            &shared_file,
            &schedule,
            jobs,
            || ClosureResult {
                node_ids: Vec::new(),
                way_ids: Vec::new(),
                relation_ids: Vec::new(),
            },
            |block, result| {
                for element in block.elements_skip_metadata() {
                    if let Element::Relation(r) = &element {
                        if !included_relation_ids.get(r.id()) {
                            continue;
                        }
                        for member in r.members() {
                            match member.id {
                                MemberId::Node(id) => result.node_ids.push(id),
                                MemberId::Way(id) => result.way_ids.push(id),
                                MemberId::Relation(id) => result.relation_ids.push(id),
                                MemberId::Unknown(..) => {}
                            }
                        }
                    }
                }
            },
            |cr| results.push(cr),
            &*host.resources,
        )?;

        // Merge phase: mutate included sets.
        for cr in results {
            if !cr.node_ids.is_empty() || !cr.way_ids.is_empty() || !cr.relation_ids.is_empty() {
                summary.has_relation = true;
            }
            for id in cr.node_ids {
                relation_dep_node_ids.set(id);
            }
            for id in cr.way_ids {
                if included_way_ids.set_if_new(id) {
                    summary.has_way = true;
                }
            }
            for id in cr.relation_ids {
                if included_relation_ids.set_if_new(id) {
                    added_relations += 1;
                }
            }
        }

        if added_relations == 0 {
            break;
        }
    }

    Ok(summary)
}

/// The `build_classify_schedule` kind filter for a reference-expansion scan.
///
/// A per-kind schedule selects blobs on the kind their *indexdata* declares,
/// which is exactly the claim `--distrust-metadata` says it will not believe.
/// Leaving these two scans narrowed was the flag's second layer: the initial
/// pass switched off blob filtering for the direct-match pass and for pass 2,
/// so a mislabelled way or relation blob had its matches found and written,
/// but its *referenced members* were never collected - the output silently
/// omitted required dependencies while the flag promised an answer that
/// depends only on element data. Under `--distrust-metadata` the schedule is
/// every OsmData blob and the per-element `if let Element::Way(_)` /
/// `Element::Relation(_)` match inside each scan does the kind selection from
/// decoded data instead.
fn kind_schedule_filter(
    kind: crate::ElementKind,
    distrust_metadata: bool,
) -> Option<crate::ElementKind> {
    if distrust_metadata { None } else { Some(kind) }
}

/// Scan all way blobs and add node refs for ways selected for output.
///
/// `distrust_metadata` widens the schedule to every OsmData blob. See
/// [`kind_schedule_filter`].
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn collect_way_node_dependencies(
    input: &Path,
    _direct_io: bool,
    distrust_metadata: bool,
    included_way_ids: &IdSet,
    skip_way_ids: Option<&IdSet>,
    relation_dep_node_ids: &mut IdSet,
    jobs: Option<usize>,
    host: &crate::host::Host,
) -> Result<()> {
    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        kind_schedule_filter(crate::ElementKind::Way, distrust_metadata),
        &host.diagnostics,
    )?;

    // Per-blob node-ref accumulation. Earlier this function used
    // `parallel_classify_accumulate` with a per-worker `IdSet`; at
    // planet's invert-match keep-rate (~99 % of ways included) each
    // worker's bitmap grew to ~1.5 GB and the cumulative across ~30
    // decode threads peaked at 24 GB anon - this per-worker bitmap shape made
    // the memory peak scale with the number of decode workers.
    // parallel_classify_phase emits per-blob `Vec<i64>` through a
    // 32-slot bounded result channel. At DEFAULT_ELEMENT_CAP
    // (crate::block_builder) ways per blob, with about 10 refs per way,
    // the queued reference vectors occupy about 20 MB. Larger input
    // blocks or more refs per way increase this estimate.
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        jobs,
        || (),
        |block, _state| {
            let mut node_ids: Vec<i64> = Vec::new();
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element
                    && included_way_ids.get(w.id())
                {
                    if let Some(skip) = skip_way_ids
                        && skip.get(w.id())
                    {
                        continue;
                    }
                    for r in w.refs() {
                        node_ids.push(r);
                    }
                }
            }
            node_ids
        },
        |_seq, node_ids| {
            for id in node_ids {
                relation_dep_node_ids.set(id);
            }
        },
        &*host.resources,
    )?;

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
    use crate::ElementKind;
    use crate::owned::TypeFilter;
    use crate::writer::{Compression, PbfWriter};

    fn filter_for(expressions: &[&str]) -> Option<BlobFilter> {
        let strs: Vec<String> = expressions.iter().map(|s| (*s).to_string()).collect();
        blob_filter_from_expressions(&parse_expressions(&strs).expect("parse"))
    }

    fn sorted_keys(keys: Option<&[Box<[u8]>]>) -> Vec<String> {
        let mut out: Vec<String> = keys
            .map(|ks| {
                ks.iter()
                    .map(|k| String::from_utf8_lossy(k).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// The blob-level tag filter is a key-set membership test, so it can carry
    /// exact keys and key prefixes and nothing else. A key matcher the set
    /// cannot express has to disable tag-level blob skipping outright -
    /// including the keys contributed by the *other* expressions, since the
    /// filter is a union and skipping on the union would skip blobs the
    /// unconstrained expression could still match.
    #[test]
    fn blob_filter_drops_tag_keys_when_a_key_matcher_cannot_be_expressed() {
        // Constrainable forms, one per matcher shape.
        let f = filter_for(&["highway=primary"]).expect("filter");
        assert_eq!(sorted_keys(f.required_tag_keys.as_deref()), vec!["highway"]);

        let f = filter_for(&["amenity,shop"]).expect("filter");
        assert_eq!(
            sorted_keys(f.required_tag_keys.as_deref()),
            vec!["amenity", "shop"],
            "every alternative of a key list is a key the blob may carry"
        );

        let f = filter_for(&["addr:*"]).expect("filter");
        assert!(f.required_tag_keys.is_none());
        assert_eq!(
            sorted_keys(f.required_tag_prefixes.as_deref()),
            vec!["addr:"]
        );

        // A substring key matcher constrains nothing expressible, so the
        // whole tag filter goes - `highway` included. Without this, widening
        // the matcher to accept `*:name` would have made it skip every blob
        // whose index lacks a literal `highway` key.
        let f = filter_for(&["highway=primary", "*:name"]);
        assert!(
            f.is_none(),
            "no type narrowing and no expressible tag key leaves nothing to filter on"
        );

        // Same, but with a type narrowing worth keeping: the blob filter
        // survives for its type bits while the tag keys are dropped.
        let f = filter_for(&["n/highway=primary", "n/*:name"]).expect("filter");
        assert!(f.want_nodes);
        assert!(!f.want_ways);
        assert!(
            f.required_tag_keys.is_none() && f.required_tag_prefixes.is_none(),
            "a substring key matcher must not leave `highway` behind as a blob-skip criterion"
        );

        // And a bare `*` key is the same story.
        let f = filter_for(&["n/amenity", "n/*"]).expect("filter");
        assert!(f.required_tag_keys.is_none());
    }

    #[test]
    fn single_pass_filters_matching_elements() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        let file = std::fs::File::create(&input).unwrap();
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(&crate::block_builder::HeaderBuilder::new().build().unwrap())
            .unwrap();
        let mut block = BlockBuilder::new();
        block.add_node(1, 0, 0, [("amenity", "bench")], None);
        block.add_node(2, 0, 0, [("name", "untagged-match")], None);
        writer
            .write_primitive_block(block.take().unwrap().unwrap())
            .unwrap();
        writer.flush().unwrap();

        let expressions = parse_expressions(&["amenity".to_owned()]).unwrap();
        let output = dir.path().join("output.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        tags_filter_single_pass(
            &input,
            &staged,
            &expressions,
            false,
            false,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &crate::host::Host::for_tests(),
        )
        .unwrap();
        staged.publish().unwrap();
        let mut ids = Vec::new();
        ElementReader::open_in(&output, false, &crate::host::Host::for_tests())
            .unwrap()
            .for_each(|element| match element {
                Element::DenseNode(node) => ids.push(node.id()),
                Element::Node(node) => ids.push(node.id()),
                Element::Way(way) => ids.push(way.id()),
                Element::Relation(relation) => ids.push(relation.id()),
            })
            .unwrap();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn element_matches_respects_type_filter() {
        let expr = Expression {
            type_filter: TypeFilter {
                nodes: false,
                ways: true,
                relations: false,
            },
            matcher: crate::tag_expr::parse_expression("highway")
                .expect("parse")
                .matcher,
        };
        let tags = [("highway", "primary")];
        let one = std::slice::from_ref(&expr);
        assert!(!element_matches(one, &tags, ElementKind::Node));
        assert!(element_matches(one, &tags, ElementKind::Way));
        assert!(!element_matches(one, &tags, ElementKind::Relation));
    }

    #[test]
    fn element_matches_or_semantics() {
        let exprs = vec![
            Expression {
                type_filter: TypeFilter::all(),
                matcher: crate::tag_expr::parse_expression("amenity")
                    .expect("parse")
                    .matcher,
            },
            Expression {
                type_filter: TypeFilter::all(),
                matcher: crate::tag_expr::parse_expression("highway=primary")
                    .expect("parse")
                    .matcher,
            },
        ];
        assert!(element_matches(
            &exprs,
            &[("amenity", "bench")],
            ElementKind::Node
        ));
        assert!(element_matches(
            &exprs,
            &[("highway", "primary")],
            ElementKind::Way
        ));
        assert!(!element_matches(
            &exprs,
            &[("name", "foo")],
            ElementKind::Node
        ));
    }
}
