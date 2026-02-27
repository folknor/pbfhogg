//! Count tag frequencies in a PBF file. Equivalent to `osmium tags-count`.

use std::path::Path;

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::{BlobFilter, Element, ElementReader, PrimitiveBlock};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type CountMap = FxHashMap<String, FxHashMap<String, u64>>;

const BATCH_SIZE: usize = 64;

/// A single tag count entry: key, value, count.
pub struct TagCount {
    pub key: String,
    pub value: String,
    pub count: u64,
}

/// Count tag key=value frequencies in a PBF file.
///
/// If `type_filter` is set, only count tags on elements of that type
/// ("node", "way", or "relation"). Results are sorted by count descending,
/// then by key, then by value. Entries below `min_count` are excluded.
///
/// Element processing is parallelized: each rayon thread accumulates a
/// local `FxHashMap`, then thread-local maps are merged via reduce.
#[hotpath::measure]
pub fn tags_count(
    path: &Path,
    min_count: u64,
    type_filter: Option<&str>,
    direct_io: bool,
) -> Result<Vec<TagCount>> {
    let reader = ElementReader::open(path, direct_io)?;
    let reader = match type_filter {
        Some("node") => reader.with_blob_filter(BlobFilter::only_nodes()),
        Some("way") => reader.with_blob_filter(BlobFilter::only_ways()),
        Some("relation") => reader.with_blob_filter(BlobFilter::only_relations()),
        _ => reader,
    };

    let filter_node = type_filter.is_none() || type_filter == Some("node");
    let filter_way = type_filter.is_none() || type_filter == Some("way");
    let filter_relation = type_filter.is_none() || type_filter == Some("relation");

    let mut counts: CountMap = FxHashMap::default();
    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    for block_result in reader.into_blocks_pipelined() {
        batch.push(block_result?);
        if batch.len() >= BATCH_SIZE {
            let batch_counts = count_batch(&batch, filter_node, filter_way, filter_relation);
            merge_counts(&mut counts, batch_counts);
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let batch_counts = count_batch(&batch, filter_node, filter_way, filter_relation);
        merge_counts(&mut counts, batch_counts);
    }

    let capacity: usize = counts.values().map(rustc_hash::FxHashMap::len).sum();
    let mut results: Vec<TagCount> = Vec::with_capacity(capacity);
    for (key, inner) in counts {
        for (value, count) in inner {
            if count >= min_count {
                results.push(TagCount {
                    key: key.clone(),
                    value,
                    count,
                });
            }
        }
    }

    results.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.value.cmp(&b.value))
    });

    Ok(results)
}

/// Count tags across a batch of blocks in parallel using fold + reduce.
fn count_batch(
    batch: &[PrimitiveBlock],
    filter_node: bool,
    filter_way: bool,
    filter_relation: bool,
) -> CountMap {
    batch
        .par_iter()
        .fold(
            FxHashMap::default,
            |mut local: CountMap, block| {
                count_block_tags(&mut local, block, filter_node, filter_way, filter_relation);
                local
            },
        )
        .reduce(FxHashMap::default, merge_two_maps)
}

/// Count tags from a single block into a local map.
fn count_block_tags(
    counts: &mut CountMap,
    block: &PrimitiveBlock,
    filter_node: bool,
    filter_way: bool,
    filter_relation: bool,
) {
    for element in block.elements() {
        let dominated = match &element {
            Element::DenseNode(_) | Element::Node(_) => filter_node,
            Element::Way(_) => filter_way,
            Element::Relation(_) => filter_relation,
        };
        if !dominated {
            continue;
        }

        match &element {
            Element::DenseNode(dn) => {
                for (k, v) in dn.tags() {
                    increment_tag(counts, k, v);
                }
            }
            Element::Node(n) => {
                for (k, v) in n.tags() {
                    increment_tag(counts, k, v);
                }
            }
            Element::Way(w) => {
                for (k, v) in w.tags() {
                    increment_tag(counts, k, v);
                }
            }
            Element::Relation(r) => {
                for (k, v) in r.tags() {
                    increment_tag(counts, k, v);
                }
            }
        }
    }
}

/// Merge map `b` into map `a`.
fn merge_two_maps(mut a: CountMap, b: CountMap) -> CountMap {
    for (key, inner_b) in b {
        let inner_a = a.entry(key).or_default();
        for (val, count) in inner_b {
            *inner_a.entry(val).or_insert(0) += count;
        }
    }
    a
}

/// Merge a complete batch result into the global accumulator.
fn merge_counts(global: &mut CountMap, batch: CountMap) {
    for (key, inner_b) in batch {
        let inner_a = global.entry(key).or_default();
        for (val, count) in inner_b {
            *inner_a.entry(val).or_insert(0) += count;
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
