//! Shared tag expression types, parser, and matcher.
//!
//! Used by `tags_filter`, `tags_filter_osc`, and `tags_count`.

use std::path::Path;

use crate::BoxResult;
use crate::owned::TypeFilter;

// ---------------------------------------------------------------------------
// Expression types
// ---------------------------------------------------------------------------

/// How one half of a tag expression - the key or the value - matches a string.
///
/// Mirrors `osmium::StringMatcher` as built by
/// `osmium-tool/src/util.cpp::get_string_matcher`, which is the reference
/// these commands are advertised against. The construction rules, in the
/// order osmium applies them:
///
/// 1. the single character `*` alone matches anything;
/// 2. otherwise, a string with no leading and no trailing `*` is an exact
///    match, or - if it contains a comma - a list of exact alternatives, each
///    whitespace-stripped;
/// 3. otherwise a trailing `*` with no leading one is a prefix match;
/// 4. anything else strips one leading and one trailing `*` and matches as a
///    substring.
///
/// The previous parser understood only rule 2 plus a key ending in `:*`, so
/// every other osmium-valid expression - `highway=res*`, `name=*strasse`,
/// `highway=*`, `amenity,shop` - parsed as an exact match on a literal
/// containing a star and matched nothing at all. Silent zero output on a
/// syntactically valid expression is the worst possible failure mode for a
/// filter; there was no error to notice and no result to question.
#[derive(Clone, Debug)]
pub(crate) enum StringMatcher {
    /// `*` - matches any string.
    Always,
    /// Exact string equality.
    Equal(String),
    /// Any one of a comma-separated list of exact alternatives.
    List(Vec<String>),
    /// `foo*` - matches strings starting with `foo`.
    Prefix(String),
    /// `*foo`, `*foo*` - matches strings containing `foo`. osmium collapses
    /// both spellings to a substring match; there is no suffix matcher.
    Substring(String),
}

impl StringMatcher {
    pub(crate) fn matches(&self, s: &str) -> bool {
        match self {
            StringMatcher::Always => true,
            StringMatcher::Equal(v) => s == v,
            StringMatcher::List(vs) => vs.iter().any(|v| v == s),
            StringMatcher::Prefix(p) => s.starts_with(p.as_str()),
            StringMatcher::Substring(sub) => s.contains(sub.as_str()),
        }
    }
}

/// What to match on a tag: a key matcher, an optional value matcher, and
/// whether the value match is inverted (`highway!=primary`).
///
/// Inversion still requires the key to match - `highway!=primary` selects
/// elements that *have* a `highway` tag whose value is not `primary`, not
/// elements lacking the tag. That is osmium's semantics and was also the
/// previous `NotValue` semantics.
#[derive(Clone, Debug)]
pub(crate) struct TagMatcher {
    pub(crate) key: StringMatcher,
    pub(crate) value: Option<StringMatcher>,
    pub(crate) invert: bool,
}

/// A parsed filter expression.
///
/// Parsing and single-tag semantics live here; consumers own the quantifier.
/// `tags_count` selects individual tags, while `tags_filter` and export select
/// whole elements. Export also accepts an empty expression list as unrestricted.
/// The fields remain crate-visible because the conservative blob prescreen in
/// `tags_filter::blob_filter_from_expressions` needs both kinds and key matcher
/// structure, including wildcard forms that disable key-based skipping.
#[derive(Clone, Debug)]
pub(crate) struct Expression {
    pub(crate) type_filter: TypeFilter,
    pub(crate) matcher: TagMatcher,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

fn is_type_prefix(prefix: &str) -> bool {
    !prefix.is_empty() && prefix.chars().all(|c| matches!(c, 'n' | 'w' | 'r'))
}

/// Split off an `n`/`w`/`r` type prefix, as `osmium::get_filter_expression`
/// does for the cases pbfhogg accepts.
///
/// A prefix that is a valid type list only once spaces are removed -
/// `  w/highway`, `w /highway` - is an error. osmium hands the raw prefix to
/// `get_types`, which throws on the space. Treating it as part of the key
/// instead, the rule for any other unrecognised prefix, would turn an
/// indented line in an `--expressions` file into the literal key
/// `w/highway`: a filter that silently matches nothing.
fn parse_type_prefix(input: &str) -> BoxResult<(TypeFilter, &str)> {
    if let Some(slash_pos) = input.find('/') {
        let prefix = &input[..slash_pos];
        let rest = &input[slash_pos + 1..];
        if is_type_prefix(prefix) {
            let tf = TypeFilter {
                nodes: prefix.contains('n'),
                ways: prefix.contains('w'),
                relations: prefix.contains('r'),
            };
            return Ok((tf, rest));
        }
        if prefix.contains(' ') && is_type_prefix(strip_spaces(prefix)) {
            return Err(format!(
                "type prefix {prefix:?} in expression {input:?} contains spaces; \
                 write it without them, e.g. \"{}/...\"",
                strip_spaces(prefix)
            )
            .into());
        }
    }
    Ok((TypeFilter::all(), input))
}

/// Strip leading and trailing spaces, as `osmium::strip_whitespace` does.
///
/// Spaces only, deliberately: osmium strips `' '` and nothing else, so a tab
/// stays part of the key there and stays part of the key here. Without this,
/// `highway = primary` read from an `--expressions` file produced the key
/// `"highway "` and matched nothing.
fn strip_spaces(s: &str) -> &str {
    s.trim_matches(' ')
}

/// Build a [`StringMatcher`] the way `get_string_matcher` does. See the
/// [`StringMatcher`] docs for the rule order.
fn parse_string_matcher(input: &str) -> StringMatcher {
    let s = strip_spaces(input);

    if s == "*" {
        return StringMatcher::Always;
    }

    let leading_star = s.starts_with('*');
    let trailing_star = s.ends_with('*');

    if !leading_star && !trailing_star {
        if s.contains(',') {
            return StringMatcher::List(
                s.split(',')
                    .map(|part| strip_spaces(part).to_string())
                    .collect(),
            );
        }
        return StringMatcher::Equal(s.to_string());
    }

    if trailing_star && !leading_star {
        return StringMatcher::Prefix(s[..s.len() - 1].to_string());
    }

    // Leading star: strip one leading and (if still present) one trailing
    // star, and match the remainder anywhere in the string.
    let mut inner = &s[1..];
    if inner.ends_with('*') {
        inner = &inner[..inner.len() - 1];
    }
    StringMatcher::Substring(inner.to_string())
}

/// Split an expression into key and optional value at the first `=`, the way
/// `osmium::get_tag_matcher` does, then build both matchers.
///
/// Splitting on the *first* `=` and then testing the key for a trailing `!`
/// is what makes `a=b!=c` mean "key `a`, value `b!=c`". Searching for `!=`
/// anywhere - the previous behaviour - made the same expression mean "key
/// `a=b`, not-value `c`", which is a key no PBF can contain.
///
/// # The `!` is tested on the raw key, before whitespace stripping
///
/// `get_tag_matcher` slices the key straight out of the expression and tests
/// `key.back() == '!'` *before* handing it to `get_string_matcher`, which is
/// the only place `strip_whitespace` runs. So the `!` counts as an inversion
/// marker only when it is the last character of the raw key - `highway !=x`
/// inverts, `highway ! = x` does not and means the literal key `"highway !"`.
///
/// Stripping first and testing after - the first version of this rewrite -
/// made `highway ! = primary` a negated match. That silently selects a
/// different set of elements than osmium does for the same expression, which
/// is worse than an error: the equivalence is advertised and there is nothing
/// in the output to notice.
fn parse_tag_matcher(input: &str) -> BoxResult<TagMatcher> {
    let (key_part, value_part) = match input.find('=') {
        Some(pos) => (&input[..pos], Some(&input[pos + 1..])),
        None => (input, None),
    };

    let mut raw_key = key_part;
    let mut invert = false;
    if value_part.is_some()
        && let Some(stripped) = raw_key.strip_suffix('!')
    {
        raw_key = stripped;
        invert = true;
    }
    let key_str = strip_spaces(raw_key);

    if key_str.is_empty() {
        return Err(if value_part.is_some() {
            "empty key in expression".into()
        } else {
            "empty expression".into()
        });
    }

    Ok(TagMatcher {
        key: parse_string_matcher(key_str),
        value: value_part.map(parse_string_matcher),
        invert,
    })
}

pub(crate) fn parse_expression(input: &str) -> BoxResult<Expression> {
    let (type_filter, tag_part) = parse_type_prefix(input)?;
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
/// Mirrors `read_expressions_file` in osmium's `command_tags_filter.cpp` and
/// `command_tags_count.cpp`, step for step:
///
/// 1. lines are split on `\n` only;
/// 2. everything from the first `#` onward is a comment, wherever it sits;
/// 3. then one trailing `\r` is removed, so a CRLF file reads like an LF one
///    (a `\r` the comment cut already removed is not looked for again);
/// 4. the line is otherwise handed to the expression parser **unmodified**.
///
/// Step 4 is the point. The parser strips spaces - and only spaces - around
/// each half of an expression, as osmium's `strip_whitespace` does, so a tab
/// stays part of the key in both tools. This reader previously called
/// `str::trim`, which strips tabs too, so a key beginning with a tab was
/// silently rewritten into a different key.
///
/// One deliberate difference: a line that is empty or spaces-only after
/// steps 2 and 3 is skipped. osmium hands it to `get_tag_matcher` too, which
/// builds a matcher for the empty key; pbfhogg rejects an empty key outright
/// (see [`parse_expression`]), and erroring on a blank line would be hostile.
pub fn read_expressions_file(path: &Path) -> BoxResult<Vec<String>> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read expressions file '{}': {e}", path.display()))?;
    Ok(contents
        .split('\n')
        .filter_map(expression_line)
        .map(ToString::to_string)
        .collect())
}

/// Normalize one raw line of an expressions file; `None` means skip it. See
/// [`read_expressions_file`] for the rules and their osmium source.
fn expression_line(raw: &str) -> Option<&str> {
    let line = match raw.find('#') {
        Some(pos) => &raw[..pos],
        None => raw,
    };
    let line = line.strip_suffix('\r').unwrap_or(line);
    if strip_spaces(line).is_empty() {
        None
    } else {
        Some(line)
    }
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

pub(crate) fn tag_matches(matcher: &TagMatcher, key: &str, value: &str) -> bool {
    if !matcher.key.matches(key) {
        return false;
    }
    match matcher.value {
        None => true,
        Some(ref vm) => vm.matches(value) != matcher.invert,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert how one expression treats one `key=value` tag pair.
    fn m(expression: &str, key: &str, value: &str) -> bool {
        let expr = parse_expression(expression).expect("parse");
        tag_matches(&expr.matcher, key, value)
    }

    #[test]
    fn key_only_matches_any_value() {
        assert_eq!(
            parse_expression("amenity").expect("parse").type_filter,
            TypeFilter::all()
        );
        assert!(m("amenity", "amenity", "bench"));
        assert!(!m("amenity", "highway", "primary"));
    }

    #[test]
    fn exact_key_and_value() {
        assert!(m("highway=primary", "highway", "primary"));
        assert!(!m("highway=primary", "highway", "residential"));
        assert!(!m("highway=primary", "railway", "primary"));
    }

    #[test]
    fn comma_list_on_values() {
        assert!(m("type=multipolygon,boundary", "type", "multipolygon"));
        assert!(m("type=multipolygon,boundary", "type", "boundary"));
        assert!(!m("type=multipolygon,boundary", "type", "route"));
    }

    /// osmium's `get_string_matcher` applies the comma-list rule to the key
    /// half as well. The old parser applied it only to values, so
    /// `amenity,shop` became one literal key that no PBF contains - zero
    /// output, no error.
    #[test]
    fn comma_list_on_keys() {
        assert!(m("amenity,shop", "amenity", "bench"));
        assert!(m("amenity,shop", "shop", "bakery"));
        assert!(!m("amenity,shop", "highway", "primary"));

        // Same rule on both halves of one expression.
        assert!(m("amenity,shop=bench,bakery", "shop", "bakery"));
        assert!(!m("amenity,shop=bench,bakery", "shop", "butcher"));
    }

    /// `foo*` is a prefix match on either half. The old parser recognised
    /// only a key ending in `:*`, so `highway=res*` was an exact match on the
    /// literal string `res*`.
    #[test]
    fn trailing_star_is_a_prefix_match() {
        assert!(m("highway=res*", "highway", "residential"));
        assert!(m("highway=res*", "highway", "rest_area"));
        assert!(!m("highway=res*", "highway", "primary"));

        assert!(m("addr:*", "addr:city", "Berlin"));
        assert!(!m("addr:*", "address", "x"));
    }

    /// A leading star means substring in osmium - there is no separate suffix
    /// matcher - and `*foo*` collapses to the same thing.
    #[test]
    fn leading_star_is_a_substring_match() {
        assert!(m("name=*strasse", "name", "Hauptstrasse"));
        assert!(m("name=*strasse", "name", "strasse am see"));
        assert!(!m("name=*strasse", "name", "Hauptweg"));

        assert!(m("*:name", "addr:name", "x"));
        assert!(m("*:name*", "de:name:left", "x"));
        assert!(!m("*:name", "name", "x"));
    }

    /// A bare `*` matches anything on either half, rather than matching a
    /// literal star character.
    ///
    /// The behavioural half of this cannot distinguish `Always` from
    /// `Substring("")` - both accept every string - so the constructed
    /// variant is asserted directly. That distinction is not academic:
    /// `get_string_matcher` reaches `always_true` only through the
    /// `size() == 1` guard, and a rule-order slip that let `*` fall through
    /// to the substring branch would be invisible to any `matches()`-only
    /// test, here and in the blob filter alike.
    #[test]
    fn bare_star_matches_anything() {
        assert!(m("highway=*", "highway", "primary"));
        assert!(m("highway=*", "highway", ""));
        assert!(m("highway=*", "highway", "not_a_star"));
        assert!(!m("highway=*", "railway", "rail"));

        assert!(m("*", "anything", "at all"));

        let expr = parse_expression("*").expect("parse");
        assert!(
            matches!(expr.matcher.key, StringMatcher::Always),
            "a lone star is osmium's always_true, not a substring of the empty string"
        );
        let expr = parse_expression("highway=*").expect("parse");
        assert!(matches!(expr.matcher.value, Some(StringMatcher::Always)));

        // `**` is two characters, so it misses the `size() == 1` guard and
        // collapses through the leading-and-trailing-star branch to a
        // substring of the empty string. Same behaviour, different variant -
        // this is the case that makes the assertion above meaningful.
        let expr = parse_expression("**").expect("parse");
        assert!(
            matches!(&expr.matcher.key, StringMatcher::Substring(s) if s.is_empty()),
            "two stars is not the always matcher"
        );
        assert!(m("**", "anything", "at all"));
    }

    #[test]
    fn negation_requires_the_key_to_be_present() {
        assert!(m("highway!=primary", "highway", "residential"));
        assert!(!m("highway!=primary", "highway", "primary"));
        assert!(
            !m("highway!=primary", "railway", "rail"),
            "an inverted value match still requires the key to match"
        );
    }

    /// Negation composes with the wildcard forms, since both halves go
    /// through the same matcher builder.
    #[test]
    fn negation_composes_with_wildcards() {
        assert!(m("highway!=res*", "highway", "primary"));
        assert!(!m("highway!=res*", "highway", "residential"));
    }

    /// The split is on the *first* `=`, and only then is a trailing `!` on
    /// the key an inversion. Searching for `!=` anywhere made `a=b!=c` mean
    /// "key `a=b`, value not `c`", which is a key no PBF can contain.
    #[test]
    fn the_split_is_on_the_first_equals() {
        assert!(m("a=b!=c", "a", "b!=c"));
        assert!(!m("a=b!=c", "a=b", "c"));
    }

    /// osmium strips spaces around each half and around each comma-list
    /// element. Without this, an expression read from an `--expressions`
    /// file as `highway = primary` yields the key `"highway "`.
    #[test]
    fn spaces_around_each_half_are_stripped() {
        assert!(m("highway = primary", "highway", "primary"));
        assert!(m("type = multipolygon, boundary", "type", "boundary"));
        assert!(m(" amenity ", "amenity", "bench"));
    }

    /// The `!` marks an inversion only when it is the last character of the
    /// *raw* key, because `get_tag_matcher` tests `key.back()` before
    /// `get_string_matcher` runs `strip_whitespace`. Every spelling below is
    /// checked against that vendored order rather than against our own.
    ///
    /// This test previously asserted `highway ! = primary` was a negation -
    /// the implementation's behaviour at the time, and not osmium's. A test
    /// that encodes the bug as the expectation cannot fail on the bug, so it
    /// pinned the divergence in place instead of catching it.
    #[test]
    fn the_bang_must_be_the_last_raw_key_character() {
        // Bang adjacent to the `=`: inversion, with the space before it
        // stripped afterwards as part of the key.
        assert!(m("highway !=primary", "highway", "residential"));
        assert!(!m("highway !=primary", "highway", "primary"));

        // Space between the bang and the `=`: the raw key ends in a space, so
        // there is no inversion and the bang is part of a literal key.
        assert!(m("highway ! = primary", "highway !", "primary"));
        assert!(!m("highway ! = primary", "highway", "residential"));

        // Same story with no space before the bang.
        assert!(m("highway! = primary", "highway!", "primary"));
        assert!(!m("highway! = primary", "highway", "residential"));

        // And a bang anywhere other than the end of the key is just a
        // character in the key.
        assert!(m("high!way=primary", "high!way", "primary"));
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
        assert!(m("addr:full/name", "addr:full/name", "x"));
    }

    #[test]
    fn parse_empty_is_error() {
        assert!(parse_expression("").is_err());
        assert!(parse_expression("   ").is_err());
    }

    #[test]
    fn parse_empty_key_in_value_expr_is_error() {
        assert!(parse_expression("=value").is_err());
        assert!(parse_expression(" = value").is_err());
    }

    #[test]
    fn parse_empty_key_in_negation_is_error() {
        assert!(parse_expression("!=value").is_err());
    }

    #[test]
    fn read_expressions_from_file() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("test_expressions.txt");
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
        // The space before the inline comment survives the reader, exactly as
        // in osmium; the parser strips it from the key.
        assert_eq!(exprs, vec!["highway=primary", "amenity ", "w/building=yes"]);
    }

    /// The comment rule is osmium's `read_expressions_file`: cut at the first
    /// `#` wherever it sits, then drop one trailing `\r`, then hand the rest
    /// over untouched. Blank and spaces-only remainders are skipped.
    #[test]
    fn expression_line_follows_the_osmium_reader() {
        assert_eq!(expression_line("highway=primary"), Some("highway=primary"));
        assert_eq!(
            expression_line("highway=primary\r"),
            Some("highway=primary")
        );
        assert_eq!(expression_line("highway # note"), Some("highway "));
        assert_eq!(expression_line("name=a#b"), Some("name=a"));
        // The comment cut removes the CR, so there is no second CR strip.
        assert_eq!(expression_line("highway #c\r"), Some("highway "));
        // A CR not at the end of the post-comment line stays.
        assert_eq!(expression_line("key\r "), Some("key\r "));
        assert_eq!(expression_line(""), None);
        assert_eq!(expression_line("\r"), None);
        assert_eq!(expression_line("   "), None);
        assert_eq!(expression_line("# comment"), None);
        assert_eq!(expression_line("   # indented comment\r"), None);
        // Tabs are content, not whitespace to strip.
        assert_eq!(expression_line("\t"), Some("\t"));
        assert_eq!(expression_line("\thighway"), Some("\thighway"));
    }

    /// A tab-led key read from a file stays tab-led, as in osmium, whose
    /// `strip_whitespace` removes spaces only. The reader used `str::trim`
    /// and turned it into the plain key.
    #[test]
    fn a_tab_led_key_from_a_file_keeps_its_tab() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("exprs.txt");
        std::fs::write(&path, "\thighway=primary\n  amenity  \r\n").expect("write");
        let exprs = read_expressions_file(&path).expect("read");
        assert_eq!(exprs, vec!["\thighway=primary", "  amenity  "]);

        let parsed = parse_expressions(&exprs).expect("parse");
        assert!(tag_matches(&parsed[0].matcher, "\thighway", "primary"));
        assert!(!tag_matches(&parsed[0].matcher, "highway", "primary"));
        // Spaces are still stripped, by the parser.
        assert!(tag_matches(&parsed[1].matcher, "amenity", "bench"));
    }

    /// osmium's `get_types` throws on a type prefix carrying a space. Taking
    /// it as part of the key - pbfhogg's rule for unrecognised prefixes -
    /// would make `  w/highway` the literal key `w/highway` and match
    /// nothing, so it is an error here too.
    #[test]
    fn a_type_prefix_with_spaces_is_an_error() {
        for expr in ["  w/highway", "w /highway", " nw /name=x"] {
            let err = parse_expression(expr).expect_err(expr).to_string();
            assert!(err.contains("contains spaces"), "{expr}: {err}");
        }
        // Spaces after the slash belong to the key half and are stripped.
        let expr = parse_expression("w/ highway").expect("parse");
        assert!(expr.type_filter.ways && !expr.type_filter.nodes);
        assert!(m("w/ highway", "highway", "primary"));
        // An unrecognised prefix is still literal key text.
        assert!(m("addr: full/name", "addr: full/name", "x"));
    }
}
