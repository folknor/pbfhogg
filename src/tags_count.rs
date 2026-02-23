//! Count tag frequencies in a PBF file. Equivalent to `osmium tags-count`.

use std::collections::HashMap;
use std::path::Path;

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
pub fn tags_count(
    path: &Path,
    min_count: u64,
    type_filter: Option<&str>,
) -> Result<Vec<TagCount>> {
    let reader = ElementReader::from_path(path)?;
    let mut counts: HashMap<(String, String), u64> = HashMap::new();

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

        let tags: Box<dyn Iterator<Item = (&str, &str)>> = match &element {
            Element::DenseNode(dn) => Box::new(dn.tags()),
            Element::Node(n) => Box::new(n.tags()),
            Element::Way(w) => Box::new(w.tags()),
            Element::Relation(r) => Box::new(r.tags()),
        };

        for (k, v) in tags {
            *counts
                .entry((k.to_string(), v.to_string()))
                .or_insert(0) += 1;
        }
    })?;

    let mut results: Vec<TagCount> = counts
        .into_iter()
        .filter(|(_, count)| *count >= min_count)
        .map(|((key, value), count)| TagCount { key, value, count })
        .collect();

    results.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.value.cmp(&b.value))
    });

    Ok(results)
}
