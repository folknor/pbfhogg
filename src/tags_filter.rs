//! Filter elements by tag expressions. Equivalent to `osmium tags-filter`.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::path::Path;

use crate::block_builder::{build_header, BlockBuilder, MemberData, MemberType, Metadata};
use crate::writer::{Compression, PbfWriter};
use crate::{BlobDecode, BlobReader, Element, RelMemberType};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Expression types
// ---------------------------------------------------------------------------

/// Which element types an expression applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
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
pub fn tags_filter(
    input: &Path,
    output: &Path,
    expression_strs: &[String],
    omit_referenced: bool,
) -> Result<TagsFilterStats> {
    let expressions = parse_expressions(expression_strs)?;
    if omit_referenced {
        tags_filter_single_pass(input, output, &expressions)
    } else {
        tags_filter_two_pass(input, output, &expressions)
    }
}

// ---------------------------------------------------------------------------
// Single-pass filter (-R mode)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn tags_filter_single_pass(
    input: &Path,
    output: &Path,
    expressions: &[Expression],
) -> Result<TagsFilterStats> {
    let mut writer = PbfWriter::to_path(output, Compression::default())?;
    let mut bb = BlockBuilder::new();
    let mut header_written = false;
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        ways_matched: 0,
        relations_matched: 0,
    };

    let reader = BlobReader::from_path(input)?;

    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                if !header_written {
                    rebuild_header(&header, &mut writer)?;
                    header_written = true;
                }
            }
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            let tags: Vec<(&str, &str)> = dn.tags().collect();
                            if element_matches(expressions, &tags, true, false, false) {
                                if !bb.can_add_node() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                let meta = dn.info().and_then(|info| {
                                    let user = info.user().ok()?;
                                    Some(Metadata {
                                        version: info.version(),
                                        timestamp: info.milli_timestamp() / 1000,
                                        changeset: info.changeset(),
                                        uid: info.uid(),
                                        user,
                                        visible: info.visible(),
                                    })
                                });
                                bb.add_node(
                                    dn.id(),
                                    dn.decimicro_lat(),
                                    dn.decimicro_lon(),
                                    &tags,
                                    meta.as_ref(),
                                );
                                stats.nodes_matched += 1;
                            }
                        }
                        Element::Node(n) => {
                            let tags: Vec<(&str, &str)> = n.tags().collect();
                            if element_matches(expressions, &tags, true, false, false) {
                                if !bb.can_add_node() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                let info = n.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_node(
                                    n.id(),
                                    n.decimicro_lat(),
                                    n.decimicro_lon(),
                                    &tags,
                                    meta.as_ref(),
                                );
                                stats.nodes_matched += 1;
                            }
                        }
                        Element::Way(w) => {
                            let tags: Vec<(&str, &str)> = w.tags().collect();
                            if element_matches(expressions, &tags, false, true, false) {
                                if !bb.can_add_way() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                let refs: Vec<i64> = w.refs().collect();
                                let info = w.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_way(w.id(), &tags, &refs, meta.as_ref());
                                stats.ways_matched += 1;
                            }
                        }
                        Element::Relation(r) => {
                            let tags: Vec<(&str, &str)> = r.tags().collect();
                            if element_matches(expressions, &tags, false, false, true) {
                                if !bb.can_add_relation() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                let members: Vec<MemberData<'_>> = r
                                    .members()
                                    .map(|m| MemberData {
                                        member_id: m.member_id,
                                        member_type: match m.member_type {
                                            RelMemberType::Node => MemberType::Node,
                                            RelMemberType::Way => MemberType::Way,
                                            RelMemberType::Relation => MemberType::Relation,
                                        },
                                        role: m.role().unwrap_or(""),
                                    })
                                    .collect();
                                let info = r.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_relation(r.id(), &tags, &members, meta.as_ref());
                                stats.relations_matched += 1;
                            }
                        }
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Two-pass filter (default mode, include references)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn tags_filter_two_pass(
    input: &Path,
    output: &Path,
    expressions: &[Expression],
) -> Result<TagsFilterStats> {
    let mut stats = TagsFilterStats {
        nodes_matched: 0,
        nodes_from_ways: 0,
        ways_matched: 0,
        relations_matched: 0,
    };

    // --- Pass 1: Collect match results and way dependencies ---
    let mut matched_node_ids: BTreeSet<i64> = BTreeSet::new();
    let mut matched_way_ids: BTreeSet<i64> = BTreeSet::new();
    let mut matched_relation_ids: BTreeSet<i64> = BTreeSet::new();
    let mut way_dep_node_ids: BTreeSet<i64> = BTreeSet::new();

    let reader = BlobReader::from_path(input)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(_) => {}
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            let tags: Vec<(&str, &str)> = dn.tags().collect();
                            if element_matches(expressions, &tags, true, false, false) {
                                matched_node_ids.insert(dn.id());
                            }
                        }
                        Element::Node(n) => {
                            let tags: Vec<(&str, &str)> = n.tags().collect();
                            if element_matches(expressions, &tags, true, false, false) {
                                matched_node_ids.insert(n.id());
                            }
                        }
                        Element::Way(w) => {
                            let tags: Vec<(&str, &str)> = w.tags().collect();
                            if element_matches(expressions, &tags, false, true, false) {
                                matched_way_ids.insert(w.id());
                                way_dep_node_ids.extend(w.refs());
                            }
                        }
                        Element::Relation(r) => {
                            let tags: Vec<(&str, &str)> = r.tags().collect();
                            if element_matches(expressions, &tags, false, false, true) {
                                matched_relation_ids.insert(r.id());
                            }
                        }
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    // --- Pass 2: Write matching elements in file order ---
    let mut writer = PbfWriter::to_path(output, Compression::default())?;
    let mut bb = BlockBuilder::new();
    let mut header_written = false;

    let reader = BlobReader::from_path(input)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                if !header_written {
                    rebuild_header(&header, &mut writer)?;
                    header_written = true;
                }
            }
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            let direct = matched_node_ids.contains(&dn.id());
                            let from_way = way_dep_node_ids.contains(&dn.id());
                            if direct || from_way {
                                if !bb.can_add_node() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                let tags: Vec<(&str, &str)> = dn.tags().collect();
                                let meta = dn.info().and_then(|info| {
                                    let user = info.user().ok()?;
                                    Some(Metadata {
                                        version: info.version(),
                                        timestamp: info.milli_timestamp() / 1000,
                                        changeset: info.changeset(),
                                        uid: info.uid(),
                                        user,
                                        visible: info.visible(),
                                    })
                                });
                                bb.add_node(
                                    dn.id(),
                                    dn.decimicro_lat(),
                                    dn.decimicro_lon(),
                                    &tags,
                                    meta.as_ref(),
                                );
                                if direct {
                                    stats.nodes_matched += 1;
                                } else {
                                    stats.nodes_from_ways += 1;
                                }
                            }
                        }
                        Element::Node(n) => {
                            let direct = matched_node_ids.contains(&n.id());
                            let from_way = way_dep_node_ids.contains(&n.id());
                            if direct || from_way {
                                if !bb.can_add_node() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                let tags: Vec<(&str, &str)> = n.tags().collect();
                                let info = n.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_node(
                                    n.id(),
                                    n.decimicro_lat(),
                                    n.decimicro_lon(),
                                    &tags,
                                    meta.as_ref(),
                                );
                                if direct {
                                    stats.nodes_matched += 1;
                                } else {
                                    stats.nodes_from_ways += 1;
                                }
                            }
                        }
                        Element::Way(w) => {
                            if matched_way_ids.contains(&w.id()) {
                                if !bb.can_add_way() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                let tags: Vec<(&str, &str)> = w.tags().collect();
                                let refs: Vec<i64> = w.refs().collect();
                                let info = w.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_way(w.id(), &tags, &refs, meta.as_ref());
                                stats.ways_matched += 1;
                            }
                        }
                        Element::Relation(r) => {
                            if matched_relation_ids.contains(&r.id()) {
                                if !bb.can_add_relation() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                let tags: Vec<(&str, &str)> = r.tags().collect();
                                let members: Vec<MemberData<'_>> = r
                                    .members()
                                    .map(|m| MemberData {
                                        member_id: m.member_id,
                                        member_type: match m.member_type {
                                            RelMemberType::Node => MemberType::Node,
                                            RelMemberType::Way => MemberType::Way,
                                            RelMemberType::Relation => MemberType::Relation,
                                        },
                                        role: m.role().unwrap_or(""),
                                    })
                                    .collect();
                                let info = r.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_relation(r.id(), &tags, &members, meta.as_ref());
                                stats.relations_matched += 1;
                            }
                        }
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn flush_block(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(&bytes)?;
    }
    Ok(())
}

fn rebuild_header(
    header: &crate::HeaderBlock,
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    let bbox = header.bbox().map(|b| (b.left, b.bottom, b.right, b.top));
    let header_bytes = build_header(
        bbox,
        header.osmosis_replication_timestamp(),
        header.osmosis_replication_sequence_number(),
        header.osmosis_replication_base_url(),
    )?;
    writer.write_header(&header_bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        assert!(!element_matches(&[expr.clone()], &tags, true, false, false));
        assert!(element_matches(&[expr.clone()], &tags, false, true, false));
        assert!(!element_matches(&[expr], &tags, false, false, true));
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
