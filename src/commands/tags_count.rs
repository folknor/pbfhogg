//! Count tag frequencies in a PBF file. Equivalent to `osmium tags-count`.

use std::path::Path;

use rustc_hash::FxHashMap;

use super::tag_expr::{tag_matches, parse_expressions, Expression};
use super::{require_indexdata, TypeFilter};
use crate::{BlobFilter, Element, PrimitiveBlock};

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
    /// Count descending (most frequent first) — default.
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
/// Uses sequential BlobReader to avoid cross-thread PrimitiveBlock
/// retention at planet scale. Diagnostic command — single-threaded
/// decode is acceptable.
#[hotpath::measure]
pub fn tags_count(
    path: &Path,
    opts: &TagCountOptions<'_>,
) -> Result<Vec<TagCount>> {
    if opts.type_filter.is_some() {
        require_indexdata(path, opts.direct_io, opts.force,
            "input PBF has no blob-level indexdata. Without indexdata, the type filter \
             is a no-op — all blobs are decompressed (significantly slower).")?;
    }

    let expressions = if opts.expressions.is_empty() {
        None
    } else {
        Some(parse_expressions(opts.expressions)?)
    };

    // Sequential reader to avoid PrimitiveBlock cross-thread retention
    // at planet scale (520K+ blobs). Diagnostic command — single-threaded
    // decode is acceptable.
    let tf = TypeFilter::from_single(opts.type_filter);
    let blob_filter = match opts.type_filter {
        Some("node") => Some(BlobFilter::only_nodes()),
        Some("way") => Some(BlobFilter::only_ways()),
        Some("relation") => Some(BlobFilter::only_relations()),
        _ => None,
    };

    let mut blob_reader = crate::blob::BlobReader::open(path, opts.direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let decompress_pool = crate::blob::DecompressPool::new();

    let mut counts: CountMap = FxHashMap::default();
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(ref filter) = blob_filter {
            if let Some(idx) = blob.index() {
                if !filter.wants_index(&idx) { continue; }
            }
        }
        let decompressed = blob.decompress_pooled(&decompress_pool)?;
        let block = PrimitiveBlock::new(decompressed)?;
        count_block_tags(&mut counts, &block, tf.nodes, tf.ways, tf.relations, &expressions);
    }

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
        let dominated = match &element {
            Element::DenseNode(_) | Element::Node(_) => filter_node,
            Element::Way(_) => filter_way,
            Element::Relation(_) => filter_relation,
        };
        if !dominated {
            continue;
        }

        let (is_node, is_way, is_relation) = match &element {
            Element::DenseNode(_) | Element::Node(_) => (true, false, false),
            Element::Way(_) => (false, true, false),
            Element::Relation(_) => (false, false, true),
        };

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
