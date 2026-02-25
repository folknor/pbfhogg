//! Count tag frequencies in a PBF file. Equivalent to `osmium tags-count`.

use std::path::Path;

use rustc_hash::FxHashMap;

use crate::{Element, ElementReader};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
/// ## Allocation strategy
///
/// Uses a two-level `FxHashMap<String, FxHashMap<String, u64>>` so that
/// lookups use `get_mut(&str)` — no allocation for existing entries.
/// Only genuinely new (key, value) pairs allocate Strings. For Denmark
/// (59M elements, ~118M tags, 3.3M distinct pairs), this reduces String
/// allocations from ~236M (two per tag) to ~6.6M (two per distinct pair).
#[hotpath::measure]
pub fn tags_count(
    path: &Path,
    min_count: u64,
    type_filter: Option<&str>,
) -> Result<Vec<TagCount>> {
    let reader = ElementReader::from_path(path)?;
    let mut counts: FxHashMap<String, FxHashMap<String, u64>> = FxHashMap::default();

    let filter_node = type_filter.is_none() || type_filter == Some("node");
    let filter_way = type_filter.is_none() || type_filter == Some("way");
    let filter_relation = type_filter.is_none() || type_filter == Some("relation");

    reader.for_each_pipelined(|element| {
        let dominated = match &element {
            Element::DenseNode(_) | Element::Node(_) => filter_node,
            Element::Way(_) => filter_way,
            Element::Relation(_) => filter_relation,
        };
        if !dominated {
            return;
        }

        // Inline tag iteration per element type to avoid Box<dyn Iterator>
        // heap allocation on every element (was 59M allocations for Denmark).
        match &element {
            Element::DenseNode(dn) => {
                for (k, v) in dn.tags() {
                    increment_tag(&mut counts, k, v);
                }
            }
            Element::Node(n) => {
                for (k, v) in n.tags() {
                    increment_tag(&mut counts, k, v);
                }
            }
            Element::Way(w) => {
                for (k, v) in w.tags() {
                    increment_tag(&mut counts, k, v);
                }
            }
            Element::Relation(r) => {
                for (k, v) in r.tags() {
                    increment_tag(&mut counts, k, v);
                }
            }
        }
    })?;

    let mut results: Vec<TagCount> = Vec::new();
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

/// Increment the count for a (key, value) pair, allocating only on first insert.
///
/// Two-level lookup via `get_mut(&str)` avoids `to_string()` when the entry
/// already exists. For typical PBF files, >98% of tags are repeats.
#[inline]
fn increment_tag(counts: &mut FxHashMap<String, FxHashMap<String, u64>>, k: &str, v: &str) {
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
