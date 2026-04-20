//! Count tag frequencies in a PBF file. Equivalent to `osmium tags-count`.

use std::path::Path;

use rustc_hash::FxHashMap;

use crate::tag_expr::{tag_matches, parse_expressions, Expression};
use super::require_indexdata;
use crate::owned::TypeFilter;
use crate::{Element, PrimitiveBlock};

use super::Result;
type CountMap = FxHashMap<String, FxHashMap<String, u64>>;

/// A single tag count entry: key, value, count.
pub struct TagCount {
    pub key: String,
    pub value: String,
    pub count: u64,
}

/// Sort order for tag count results.
#[derive(Clone, Copy, Default)]
pub enum TagCountSort {
    /// Count descending (most frequent first) - default.
    #[default]
    CountDesc,
    /// Count ascending (least frequent first).
    CountAsc,
    /// Key name ascending, then value ascending.
    NameAsc,
    /// Key name descending, then value descending.
    NameDesc,
}

/// Options for `tags_count`.
pub struct TagCountOptions<'a> {
    pub min_count: u64,
    pub max_count: Option<u64>,
    pub sort: TagCountSort,
    pub expressions: &'a [String],
    pub type_filter: Option<&'a str>,
    pub direct_io: bool,
    pub force: bool,
    /// Parallel worker count. `0` picks from `available_parallelism()`;
    /// higher values override that.
    pub jobs: usize,
}

/// Count tag key=value frequencies in a PBF file.
///
/// If `type_filter` is set, only count tags on elements of that type
/// ("node", "way", or "relation"). Results are sorted by count descending,
/// then by key, then by value. Entries below `min_count` are excluded.
///
/// If `expressions` is non-empty, only tags matching at least one
/// expression are counted (same syntax as `tags-filter`).
///
/// Parallel workers via `parallel_classify_phase`: each worker emits a
/// per-blob `CountMap` (bounded by the blob's ~8 000 elements); the
/// consumer thread merges per-blob maps into a single global. The
/// accumulate-style alternative (per-worker map held across every
/// blob a worker sees) hit 26.8 GB peak anon at planet because every
/// worker held roughly the global cardinality by the end. Per-blob
/// emission keeps peak anon at a small multiple of one blob's
/// distinct tags (~few MB) plus the single global map.
#[hotpath::measure]
pub fn tags_count(
    path: &Path,
    opts: &TagCountOptions<'_>,
) -> Result<Vec<TagCount>> {
    // Need indexdata either for the type-filter schedule or (when there
    // is no type filter) simply for the all-kinds schedule the parallel
    // path uses. `require_indexdata` gracefully accepts `force`.
    require_indexdata(path, opts.direct_io, opts.force,
        "input PBF has no blob-level indexdata. Without indexdata, the type filter \
         is a no-op - all blobs are decompressed (significantly slower).")?;

    let expressions = if opts.expressions.is_empty() {
        None
    } else {
        Some(parse_expressions(opts.expressions)?)
    };

    let tf = TypeFilter::from_single(opts.type_filter);
    let kind_filter = match opts.type_filter {
        Some("node") => Some(crate::blob_meta::ElemKind::Node),
        Some("way") => Some(crate::blob_meta::ElemKind::Way),
        Some("relation") => Some(crate::blob_meta::ElemKind::Relation),
        _ => None,
    };

    crate::debug::emit_marker("TAGSCOUNT_START");

    let (schedule, shared_file) =
        crate::scan::classify::build_classify_schedule(path, kind_filter)?;
    let thread_override = (opts.jobs > 0).then_some(opts.jobs);
    let mut counts: CountMap = FxHashMap::default();

    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        thread_override,
        || (),
        |block, _s| {
            let mut local: CountMap = FxHashMap::default();
            count_block_tags(
                &mut local,
                block,
                tf.nodes,
                tf.ways,
                tf.relations,
                &expressions,
            );
            local
        },
        |_seq, per_blob| {
            merge_counts(&mut counts, per_blob);
        },
    )?;

    let capacity: usize = counts.values().map(rustc_hash::FxHashMap::len).sum();
    let mut results: Vec<TagCount> = Vec::with_capacity(capacity);
    for (key, inner) in counts {
        for (value, count) in inner {
            if count >= opts.min_count && opts.max_count.is_none_or(|max| count <= max) {
                results.push(TagCount {
                    key: key.clone(),
                    value,
                    count,
                });
            }
        }
    }

    results.sort_by(|a, b| match opts.sort {
        TagCountSort::CountDesc => b
            .count
            .cmp(&a.count)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.value.cmp(&b.value)),
        TagCountSort::CountAsc => a
            .count
            .cmp(&b.count)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.value.cmp(&b.value)),
        TagCountSort::NameAsc => a
            .key
            .cmp(&b.key)
            .then_with(|| a.value.cmp(&b.value))
            .then_with(|| b.count.cmp(&a.count)),
        TagCountSort::NameDesc => b
            .key
            .cmp(&a.key)
            .then_with(|| b.value.cmp(&a.value))
            .then_with(|| b.count.cmp(&a.count)),
    });

    crate::debug::emit_marker("TAGSCOUNT_END");
    Ok(results)
}


/// Check if a tag matches any expression (respecting the element's type).
fn matches_expression(expressions: &[Expression], key: &str, value: &str, is_node: bool, is_way: bool, is_relation: bool) -> bool {
    for expr in expressions {
        let type_ok = (is_node && expr.type_filter.nodes)
            || (is_way && expr.type_filter.ways)
            || (is_relation && expr.type_filter.relations);
        if type_ok && tag_matches(&expr.matcher, key, value) {
            return true;
        }
    }
    false
}

/// Count tags from a single block into a local map.
fn count_block_tags(
    counts: &mut CountMap,
    block: &PrimitiveBlock,
    filter_node: bool,
    filter_way: bool,
    filter_relation: bool,
    expressions: &Option<Vec<Expression>>,
) {
    for element in block.elements_skip_metadata() {
        let (dominated, is_node, is_way, is_relation) = match &element {
            Element::DenseNode(_) | Element::Node(_) => (filter_node, true, false, false),
            Element::Way(_) => (filter_way, false, true, false),
            Element::Relation(_) => (filter_relation, false, false, true),
        };
        if !dominated {
            continue;
        }

        match &element {
            Element::DenseNode(dn) => {
                for (k, v) in dn.tags() {
                    if expressions.as_ref().is_none_or(|e| matches_expression(e, k, v, is_node, is_way, is_relation)) {
                        increment_tag(counts, k, v);
                    }
                }
            }
            Element::Node(n) => {
                for (k, v) in n.tags() {
                    if expressions.as_ref().is_none_or(|e| matches_expression(e, k, v, is_node, is_way, is_relation)) {
                        increment_tag(counts, k, v);
                    }
                }
            }
            Element::Way(w) => {
                for (k, v) in w.tags() {
                    if expressions.as_ref().is_none_or(|e| matches_expression(e, k, v, is_node, is_way, is_relation)) {
                        increment_tag(counts, k, v);
                    }
                }
            }
            Element::Relation(r) => {
                for (k, v) in r.tags() {
                    if expressions.as_ref().is_none_or(|e| matches_expression(e, k, v, is_node, is_way, is_relation)) {
                        increment_tag(counts, k, v);
                    }
                }
            }
        }
    }
}


/// Increment the count for a (key, value) pair, allocating only on first insert.
#[inline]
fn increment_tag(counts: &mut CountMap, k: &str, v: &str) {
    if let Some(inner) = counts.get_mut(k) {
        if let Some(count) = inner.get_mut(v) {
            *count += 1;
        } else {
            inner.insert(v.to_string(), 1);
        }
    } else {
        let mut inner = FxHashMap::default();
        inner.insert(v.to_string(), 1);
        counts.insert(k.to_string(), inner);
    }
}

/// Merge a per-worker CountMap into the global totals. Sums counts for
/// keys present in both; moves unique entries across. Move-based for
/// the inner maps to avoid re-hashing value strings.
fn merge_counts(global: &mut CountMap, worker: CountMap) {
    for (key, worker_inner) in worker {
        match global.get_mut(&key) {
            Some(global_inner) => {
                for (value, count) in worker_inner {
                    *global_inner.entry(value).or_insert(0) += count;
                }
            }
            None => {
                global.insert(key, worker_inner);
            }
        }
    }
}
