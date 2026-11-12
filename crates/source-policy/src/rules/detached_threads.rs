//! No `thread::spawn` handle is thrown away.
//!
//! A detached thread outlives whatever started it: in a test, a thread left
//! running past a failed wait keeps working under later tests on a fixture
//! that may be gone. A wait that must bound a possible hang is a
//! `pbfhogg_test_scratch::HangBound`, which ends the binary instead, and the
//! thread's handle is kept and joined. Production code starts threads through
//! the library's joined builders, which clippy's disallowed methods enforce.
//!
//! This is a lexical tripwire, not an ownership proof: it refuses a
//! `thread::spawn(...)` call (optionally `std::`-qualified) that is a whole
//! expression statement or the whole initializer of `let _ =`. A handle bound
//! to a name and then dropped, a `Builder` spawn, and a spawn inside macro
//! input it cannot see as a statement are outside it.

use crate::Corpus;
use crate::lex::{Source, Tok};

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found: Vec<String> = super::require_files(corpus, "crates/", "detached threads")
        .into_iter()
        .collect();
    for (path, source) in corpus.under("crates/") {
        for line in discarded(source) {
            found.push(format!(
                "{path}:{line}: a thread::spawn handle is discarded; keep and join it, \
                 and bound a possible hang with pbfhogg_test_scratch::HangBound"
            ));
        }
    }
    found
}

/// Lines of every `thread::spawn(...)` whose handle is discarded.
pub fn discarded(source: &Source) -> Vec<usize> {
    let code = source.code();
    let mut out = Vec::new();
    for i in 0..code.len() {
        if !is_spawn_path(source, &code, i) {
            continue;
        }
        let Some(start) = path_start(source, &code, i) else {
            continue;
        };
        let Some(close) = matching_paren(source, &code, i + 4) else {
            continue;
        };
        if !code.get(close + 1).is_some_and(|t| source.is_punct(t, ";")) {
            continue;
        }
        let before = |back: usize| start.checked_sub(back).and_then(|j| code.get(j));
        let statement = match before(1) {
            None => true,
            Some(t) => ["{", "}", ";"].iter().any(|p| source.is_punct(t, p)),
        };
        let let_underscore = before(1).is_some_and(|t| source.is_punct(t, "="))
            && before(2).is_some_and(|t| source.is_word(t, "_") || source.text_of(t) == "_")
            && before(3).is_some_and(|t| source.is_word(t, "let"));
        if statement || let_underscore {
            out.push(code[i].line);
        }
    }
    out
}

/// `thread :: spawn (` starting at `i`.
fn is_spawn_path(source: &Source, code: &[Tok], i: usize) -> bool {
    code.get(i).is_some_and(|t| source.is_word(t, "thread"))
        && code.get(i + 1).is_some_and(|t| source.is_punct(t, ":"))
        && code.get(i + 2).is_some_and(|t| source.is_punct(t, ":"))
        && code.get(i + 3).is_some_and(|t| source.is_word(t, "spawn"))
        && code.get(i + 4).is_some_and(|t| source.is_punct(t, "("))
}

/// The first token of the path ending in `thread` at `i`: `std ::` and a
/// leading `::` are part of it. `None` when another path segment or a method
/// receiver precedes it, which this rule does not judge.
fn path_start(source: &Source, code: &[Tok], i: usize) -> Option<usize> {
    let colons = |j: usize| {
        j >= 2 && source.is_punct(&code[j - 1], ":") && source.is_punct(&code[j - 2], ":")
    };
    let mut start = i;
    if colons(start) {
        let segment = start.checked_sub(3)?;
        if !source.is_word(&code[segment], "std") {
            return None;
        }
        start = segment;
        if colons(start) {
            start -= 2;
        }
    }
    if start > 0 && source.is_punct(&code[start - 1], ".") {
        return None;
    }
    Some(start)
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

#[cfg(test)]
mod tests {
    use super::discarded;
    use crate::lex::Source;

    fn lines(src: &str) -> Vec<usize> {
        discarded(&Source::new(src.to_owned()))
    }

    #[test]
    fn statement_and_let_underscore_spawns_are_refused() {
        for src in [
            "fn f() { std::thread::spawn(move || g()); }",
            "fn f() { thread::spawn(|| { g(); }); }",
            "fn f() { let x = 1; ::std::thread::spawn(|| ()); }",
            "fn f() { let _ = std::thread::spawn(|| ()); }",
        ] {
            assert_eq!(lines(src).len(), 1, "{src}");
        }
    }

    #[test]
    fn kept_joined_and_returned_handles_pass() {
        for src in [
            "fn f() { let h = std::thread::spawn(|| ()); h.join(); }",
            "fn f() { std::thread::spawn(|| ()).join().unwrap(); }",
            "fn f() -> JoinHandle<()> { std::thread::spawn(|| ()) }",
            "fn f() { let _ = std::thread::spawn(|| ()).join(); }",
            "fn f() { v.push(thread::spawn(|| ())); }",
            "fn f() { scope.spawn(|| ()); }",
            "// std::thread::spawn(|| ());",
            "fn f() { let s = \"thread::spawn(|| ());\"; }",
        ] {
            assert!(lines(src).is_empty(), "{src}");
        }
    }
}
