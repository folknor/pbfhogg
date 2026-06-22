//! Shared tag expression types, parser, and matcher.
//!
//! Used by `tags_filter`, `tags_filter_osc`, and `tags_count`.

use std::io::BufRead;
use std::path::Path;

use crate::BoxResult;
use crate::owned::TypeFilter;

// ---------------------------------------------------------------------------
// Expression types
// ---------------------------------------------------------------------------

/// What to match on a tag.
#[derive(Clone, Debug)]
pub(crate) enum TagMatcher {
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
pub(crate) struct Expression {
    pub(crate) type_filter: TypeFilter,
    pub(crate) matcher: TagMatcher,
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

fn parse_tag_matcher(input: &str) -> BoxResult<TagMatcher> {
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

pub(crate) fn parse_expression(input: &str) -> BoxResult<Expression> {
    let (type_filter, tag_part) = parse_type_prefix(input);
    let matcher = parse_tag_matcher(tag_part)?;
    Ok(Expression {
        type_filter,
        matcher,
    })
}

pub(crate) fn parse_expressions(inputs: &[String]) -> BoxResult<Vec<Expression>> {
    inputs.iter().map(|s| parse_expression(s)).collect()
}

/// Read expressions from a file, one per line.
///
/// - Lines starting with `#` (after optional whitespace) are comments.
/// - Inline `#` comments are supported (everything from first `#` onward is stripped).
/// - Blank lines are ignored.
/// - Windows line endings (`\r\n`) are handled.
pub fn read_expressions_file(path: &Path) -> BoxResult<Vec<String>> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("could not open expressions file '{}': {e}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut expressions = Vec::new();
    for line in reader.lines() {
        let line =
            line.map_err(|e| format!("error reading expressions file '{}': {e}", path.display()))?;
        // Strip inline comments
        let line = match line.find('#') {
            Some(pos) => &line[..pos],
            None => &line,
        };
        // Strip whitespace and trailing \r
        let line = line.trim();
        if !line.is_empty() {
            expressions.push(line.to_string());
        }
    }
    Ok(expressions)
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

pub(crate) fn tag_matches(matcher: &TagMatcher, key: &str, value: &str) -> bool {
    match matcher {
        TagMatcher::KeyOnly { key: k } => key == k,
        TagMatcher::KeyPrefix { prefix } => key.starts_with(prefix.as_str()),
        TagMatcher::ExactValue { key: k, value: v } => key == k && value == v,
        TagMatcher::MultiValue { key: k, values } => key == k && values.iter().any(|v| v == value),
        TagMatcher::NotValue { key: k, value: v } => key == k && value != v,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_only() {
        let expr = parse_expression("amenity").expect("parse");
        assert_eq!(expr.type_filter, TypeFilter::all());
        assert!(matches!(
            expr.matcher,
            TagMatcher::KeyOnly { ref key } if key == "amenity"
        ));
    }

    #[test]
    fn parse_exact_value() {
        let expr = parse_expression("highway=primary").expect("parse");
        assert!(matches!(
            expr.matcher,
            TagMatcher::ExactValue { ref key, ref value }
                if key == "highway" && value == "primary"
        ));
    }

    #[test]
    fn parse_multi_value() {
        let expr = parse_expression("type=multipolygon,boundary").expect("parse");
        assert!(matches!(
            expr.matcher,
            TagMatcher::MultiValue { ref key, ref values }
                if key == "type" && values == &["multipolygon", "boundary"]
        ));
    }

    #[test]
    fn parse_negation() {
        let expr = parse_expression("highway!=primary").expect("parse");
        assert!(matches!(
            expr.matcher,
            TagMatcher::NotValue { ref key, ref value }
                if key == "highway" && value == "primary"
        ));
    }

    #[test]
    fn parse_wildcard_prefix() {
        let expr = parse_expression("addr:*").expect("parse");
        assert!(matches!(
            expr.matcher,
            TagMatcher::KeyPrefix { ref prefix } if prefix == "addr:"
        ));
    }

    #[test]
    fn parse_type_prefix_node() {
        let expr = parse_expression("n/amenity").expect("parse");
        assert!(expr.type_filter.nodes);
        assert!(!expr.type_filter.ways);
        assert!(!expr.type_filter.relations);
    }

    #[test]
    fn parse_type_prefix_nw() {
        let expr = parse_expression("nw/highway=primary").expect("parse");
        assert!(expr.type_filter.nodes);
        assert!(expr.type_filter.ways);
        assert!(!expr.type_filter.relations);
    }

    #[test]
    fn parse_type_prefix_nwr() {
        let expr = parse_expression("nwr/name").expect("parse");
        assert_eq!(expr.type_filter, TypeFilter::all());
    }

    #[test]
    fn parse_slash_in_key_not_type_prefix() {
        // "addr:full/name" has non-nwr chars before '/', so no type prefix
        let expr = parse_expression("addr:full/name").expect("parse");
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
    fn read_expressions_from_file() {
        let dir = std::env::temp_dir().join("pbfhogg_test_expr");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("test_expressions.txt");
        std::fs::write(
            &path,
            "# comment line\n\
             highway=primary\n\
             \n\
             amenity # inline comment\n\
             \r\n\
             w/building=yes\n",
        )
        .expect("write");
        let exprs = read_expressions_file(&path).expect("read");
        assert_eq!(exprs, vec!["highway=primary", "amenity", "w/building=yes"]);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
