//! Every timed wait says why its expiry is not an inference.
//!
//! A test never infers that another thread blocked from elapsed time: it
//! waits, bounded, for an observation made at the wait itself
//! (`reference/testing.md`, "Test time bounds"). The shapes that break that
//! rule are a sleep before an assertion and a timed receive or condvar wait
//! whose expiry is taken as the answer. This rule makes each one visible to
//! review: every code identifier in [`TIMED`], anywhere in the corpus,
//! production included, needs a `// wait-deliberate: <reason>` line comment
//! on its line or alone on the line above it. Matching identifiers rather
//! than calls catches an aliased import (`use std::thread::sleep as nap`), a
//! function reference and a UFCS path at the name itself.
//!
//! One shape passes unmarked: `recv_timeout` or `recv_deadline` called as a
//! method whose balanced call is followed directly by `.unwrap()` or
//! `.expect(`. Expiry panics at that receive, so the timeout is a failure
//! bound. That says nothing about whether the received message establishes
//! what the test then asserts, and a caller that catches the panic is
//! outside it.
//!
//! A spelling tripwire, not enforcement of the prose rule: a wrapper
//! function, macro expansion, and spins on `Instant`, `yield_now` or
//! `try_recv` are outside it, and a marker's reason is checked only for
//! being present.

use std::collections::BTreeSet;

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

/// The names of std's timed waits.
pub const TIMED: &[&str] = &[
    "sleep",
    "sleep_until",
    "sleep_ms",
    "park_timeout",
    "park_timeout_ms",
    "recv_timeout",
    "recv_deadline",
    "wait_timeout",
    "wait_timeout_ms",
    "wait_timeout_while",
];

/// The receives whose `.unwrap()` or `.expect(` makes expiry a panic.
const RECEIVES: &[&str] = &["recv_timeout", "recv_deadline"];

const MARKER: &str = "wait-deliberate:";

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found: Vec<String> = super::require_files(corpus, "crates/", "timed waits")
        .into_iter()
        .collect();
    for (path, source) in corpus.under("crates/") {
        for (line, name) in unmarked(source) {
            found.push(format!(
                "{path}:{line}: `{name}` is a timed wait; wait for an observation made at \
                 the wait itself, or say why with `// wait-deliberate: <reason>`"
            ));
        }
    }
    found
}

/// Every timed-wait identifier no marker covers, with its line.
pub fn unmarked(source: &Source) -> Vec<(usize, String)> {
    let code = source.code();
    let marked = marked_lines(source);
    let mut out = Vec::new();
    for (i, tok) in code.iter().enumerate() {
        let Some(name) = source.name(tok) else {
            continue;
        };
        if !TIMED.contains(&name) || marked.contains(&tok.line) {
            continue;
        }
        if RECEIVES.contains(&name) && panics_on_expiry(source, &code, i) {
            continue;
        }
        out.push((tok.line, name.to_owned()));
    }
    out
}

/// `. name ( ... ) . unwrap ( )` or `. name ( ... ) . expect (` at `i`.
fn panics_on_expiry(source: &Source, code: &[Tok], i: usize) -> bool {
    let is = |j: usize, p: &str| code.get(j).is_some_and(|t| source.is_punct(t, p));
    if i == 0 || !is(i - 1, ".") || !is(i + 1, "(") {
        return false;
    }
    let Some(close) = matching_paren(source, code, i + 1) else {
        return false;
    };
    if !is(close + 1, ".") || !is(close + 3, "(") {
        return false;
    }
    let Some(method) = code.get(close + 2).and_then(|t| source.name(t)) else {
        return false;
    };
    match method {
        "unwrap" => is(close + 4, ")"),
        "expect" => true,
        _ => false,
    }
}

fn matching_paren(source: &Source, code: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (j, tok) in code.iter().enumerate().skip(open) {
        if source.is_punct(tok, "(") {
            depth += 1;
        } else if source.is_punct(tok, ")") {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(j);
            }
        }
    }
    None
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
    use super::unmarked;
    use crate::lex::Source;

    fn names(src: &str) -> Vec<String> {
        unmarked(&Source::new(src.to_owned()))
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    #[test]
    fn timed_waits_are_refused_in_every_spelling() {
        for src in [
            "fn f() { std::thread::sleep(d); }",
            "use std::thread::sleep as nap;",
            "fn f() { let p = std::thread::sleep; }",
            "fn f() { r#sleep(d); }",
            "fn f() { std::thread::park_timeout(d); }",
            "fn f() { Receiver::recv_timeout(&rx, d).unwrap(); }",
            "fn f() { let _ = rx.recv_timeout(d); }",
            "fn f() { match rx.recv_timeout(d) { _ => {} } }",
            "fn f() { rx.recv_timeout(d).unwrap_or(0); }",
            "fn f() { rx.recv_timeout(d).unwrap(x); }",
            "fn f() { cv.wait_timeout(g, d).unwrap(); }",
            "fn f() { cv.wait_timeout_while(g, d, |s| *s).expect(\"lock\"); }",
            "fn f() { let recv_timeout = d; }",
            "fn f() { std::thread::sleep(d); } // wait-deliberate:",
        ] {
            assert_eq!(names(src).len(), 1, "{src}");
        }
    }

    #[test]
    fn panicking_receives_and_marked_waits_pass() {
        for src in [
            "fn f() { rx.recv_timeout(d).unwrap(); }",
            "fn f() { rx.recv_timeout(g(h(d))).expect(\"msg\"); }",
            "fn f() {\n    rx\n        .recv_timeout(d)\n        .expect(\"msg\");\n}",
            "fn f() { rx.recv_deadline(t).unwrap(); }",
            "fn f() { std::thread::sleep(d); } // wait-deliberate: a reason",
            "fn f() {\n    // wait-deliberate: a reason\n    std::thread::sleep(d);\n}",
            "// std::thread::sleep(d);",
            "fn f() { let s = \"std::thread::sleep(d)\"; }",
            "fn f() { asleep(d); sleeper(); }",
        ] {
            assert!(names(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn a_marker_covers_only_its_line_or_the_next() {
        for src in [
            "fn f() {\n    // wait-deliberate: a reason\n\n    std::thread::sleep(d);\n}",
            "fn f() { let a = 1; // wait-deliberate: a reason\n    std::thread::sleep(d);\n}",
            "fn f() { /* // wait-deliberate: a reason */ std::thread::sleep(d); }",
        ] {
            assert_eq!(names(src).len(), 1, "{src}");
        }
    }
}
