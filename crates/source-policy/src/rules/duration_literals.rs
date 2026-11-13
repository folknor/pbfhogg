//! A duration is a named tunable, never a literal at its use.
//!
//! Every time bound and interval lives in `crates/lib/src/tunables.rs` with
//! its rationale (production values, and the "Test time bounds" a test
//! names by kind), so a `Duration` built from a numeric literal anywhere
//! else is refused. Two exceptions: the owners below, and a site carrying
//! `// fixture-time: <reason>` on its line or alone on the line above,
//! saying the value is data (a fixture clock, arithmetic under test), not a
//! wait.
//!
//! A centralisation tripwire, not wait enforcement: a named integer, an
//! alias, arithmetic and a raw timeout argument pass it. The waits
//! themselves are `timed_waits`' business.

use std::collections::BTreeSet;

use crate::Corpus;
use crate::lex::{Kind, Source};

/// Files that own durations: each independent tool's tunables, plus the
/// hang bound's own tests, which cannot depend on the library.
const OWNERS: &[&str] = &[
    "crates/lib/src/tunables.rs",
    "crates/library/src/tunables.rs",
    "crates/verify/src/tunables.rs",
    "crates/test-scratch/src/hang.rs",
];

const CONSTRUCTORS: &[&str] = &[
    "from_secs",
    "from_millis",
    "from_micros",
    "from_nanos",
    "from_secs_f32",
    "from_secs_f64",
    "from_mins",
    "from_hours",
    "new",
];

const MARKER: &str = "fixture-time:";

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found: Vec<String> = super::require_files(corpus, "crates/", "duration literals")
        .into_iter()
        .collect();
    for owner in OWNERS {
        if !corpus.files.contains_key(*owner) {
            found.push(format!("{owner}: a duration owner is missing"));
        }
    }
    for (path, source) in corpus.under("crates/") {
        if OWNERS.contains(&path) {
            continue;
        }
        for line in literal_lines(source) {
            found.push(format!(
                "{path}:{line}: a literal duration; name it in crates/lib/src/tunables.rs, or \
                 mark fixture data with `// fixture-time: <reason>`"
            ));
        }
    }
    found
}

/// Lines of `Duration :: <constructor> ( <number>` no marker covers.
pub fn literal_lines(source: &Source) -> Vec<usize> {
    let code = source.code();
    let marked = marked_lines(source);
    let mut out = Vec::new();
    for i in 0..code.len() {
        let is_name =
            |j: usize, word: &str| code.get(j).is_some_and(|t| source.name(t) == Some(word));
        let is_punct = |j: usize, p: &str| code.get(j).is_some_and(|t| source.is_punct(t, p));
        if !is_name(i, "Duration") || !is_punct(i + 1, ":") || !is_punct(i + 2, ":") {
            continue;
        }
        let Some(ctor) = code.get(i + 3).and_then(|t| source.name(t)) else {
            continue;
        };
        if !CONSTRUCTORS.contains(&ctor) || !is_punct(i + 4, "(") {
            continue;
        }
        let literal = code
            .get(i + 5)
            .is_some_and(|t| matches!(t.kind, Kind::Int | Kind::Float));
        if literal && !marked.contains(&code[i].line) {
            out.push(code[i].line);
        }
    }
    out
}

fn is_marker(comment: &str) -> bool {
    comment
        .strip_prefix("//")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix(MARKER))
        .is_some_and(|reason| !reason.trim_start().is_empty())
}

/// Lines a marker covers: its own, and the next when it stands alone.
fn marked_lines(source: &Source) -> BTreeSet<usize> {
    let mut code_lines = BTreeSet::new();
    for tok in source.code() {
        code_lines.extend(tok.line..=tok.end_line);
    }
    let mut marked = BTreeSet::new();
    for tok in &source.toks {
        if tok.kind == Kind::LineComment && is_marker(source.text_of(tok)) {
            marked.insert(tok.line);
            if !code_lines.contains(&tok.line) {
                marked.insert(tok.line + 1);
            }
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::literal_lines;
    use crate::lex::Source;

    fn lines(src: &str) -> Vec<usize> {
        literal_lines(&Source::new(src.to_owned()))
    }

    #[test]
    fn literal_durations_are_refused() {
        for src in [
            "fn f() { let d = Duration::from_millis(1); }",
            "fn f() { let d = std::time::Duration::from_secs(10); }",
            "fn f() { let d = Duration::from_secs_f64(0.5); }",
            "fn f() { let d = Duration::new(1, 0); }",
            "fn f() { let d = Duration::from_secs(1); } // fixture-time:",
        ] {
            assert_eq!(lines(src).len(), 1, "{src}");
        }
    }

    #[test]
    fn named_zero_and_marked_durations_pass() {
        for src in [
            "fn f() { let d = Duration::from_secs(BOUND); }",
            "fn f() { let d = Duration::ZERO; }",
            "fn f() { let d = Duration::from_secs(1); } // fixture-time: a fixture clock",
            "fn f() {\n    // fixture-time: a fixture clock\n    let d = Duration::from_secs(1);\n}",
            "// Duration::from_secs(1)",
            "fn f() { let s = \"Duration::from_secs(1)\"; }",
        ] {
            assert!(lines(src).is_empty(), "{src}");
        }
    }
}
