//! Every loop that probes for a state change says what ends or wakes it.
//!
//! `timed_waits` refuses sleeps and spin primitives and marks timed waits,
//! but a poll needs neither: a loop around `try_recv`, an atomic `load` or
//! an `Instant` deadline spins without naming a banned call. This rule makes
//! each such loop visible to review. A `loop`, `while` or `for` whose own
//! tokens (header and body, closures included, `fn` items nested in it
//! excluded: defining a function does not run it per pass) name one of
//! [`PROBES`] or call `.load(` needs a `// loop-ends: <reason>` line comment
//! on the line of its keyword, or opening the run of line comments directly
//! above it (a reason may wrap onto plain comment lines). One exception:
//! `.load(` in a `for` whose header spells no endless source (see
//! `spells_endless_source`) passes unmarked, because a `for` over a
//! collection or a bounded range reads atomic bitsets and counters as data;
//! a `for` over a recognized endless spelling keeps the obligation.
//!
//! The obligation reaches every enclosing loop, not only the innermost: an
//! outer loop that reruns a draining inner loop polls on its own, and the
//! inner loop's reason cannot discharge it. The reason says why another
//! pass is justified: the blocking operation and what notifies it, or that
//! the loop never waits and how each pass progresses or exits. One neutral
//! marker, because a loop can do work, then block, then end on
//! cancellation, and a work loop must not carry a marker claiming a
//! notifier.
//!
//! Lexer tokens discover the obligations: every loop keyword and every
//! probe. syn only narrows them. A keyword syn parses as a loop takes that
//! loop's exact extent. A keyword syn does not parse as a loop (`impl A for
//! B`, a `for<'a>` bound) is exempt, unless it sits in tokens syn leaves
//! opaque (a macro invocation or definition, an attribute): there the whole
//! opaque region is the loop's extent, which can add probes but never drops
//! one. A file syn cannot parse is an error.
//!
//! A spelling tripwire that forces a reviewed reason, not proof of blocking
//! control flow: a probe behind a helper function, code a macro assembles
//! from elsewhere, a hand-written endless iterator, or a marker kept after
//! its loop's blocking call was removed are outside it, and a reason is
//! checked only for being present.

use std::collections::BTreeSet;
use std::ops::Range;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

/// The names that probe for a state change without blocking, or read the
/// clock a deadline loop re-checks.
pub const PROBES: &[&str] = &[
    "try_recv",
    "try_lock",
    "try_wait",
    "try_send",
    "Instant",
    "elapsed",
    "duration_since",
    "saturating_duration_since",
];

/// Iterator sources that can run without end (`from_fn` and `successors`
/// may also stop; naming one is enough to keep the obligation).
const ENDLESS: &[&str] = &["repeat", "repeat_with", "from_fn", "successors", "cycle"];

const MARKER: &str = "loop-ends:";

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found: Vec<String> = super::require_files(corpus, "crates/", "loop waits")
        .into_iter()
        .collect();
    for (path, source) in corpus.under("crates/") {
        match unmarked(source) {
            Ok(loops) => {
                for (line, keyword, probe) in loops {
                    found.push(format!(
                        "{path}:{line}: this `{keyword}` loop names `{probe}`; say what ends \
                         or wakes each pass with `// loop-ends: <reason>`, or wait for a \
                         notification"
                    ));
                }
            }
            Err(e) => found.push(format!(
                "{path}: loop waits: syn cannot parse the file: {e}"
            )),
        }
    }
    found
}

/// A loop syn parsed: its whole extent, and for a `for` the end of its
/// header (the body's open brace).
struct Parsed {
    extent: Range<usize>,
    header_end: Option<usize>,
}

/// What syn knows about one file, by byte offset.
#[derive(Default)]
struct Structure {
    /// Loops keyed by the offset of their keyword.
    loops: Vec<(usize, Parsed)>,
    /// `fn` items with bodies.
    fns: Vec<Range<usize>>,
    /// Token regions syn does not parse into code.
    opaque: Vec<Range<usize>>,
}

struct Walk<'s> {
    source: &'s Source,
    out: Structure,
}

impl Walk<'_> {
    fn at(&self, span: proc_macro2::Span) -> usize {
        let start = span.start();
        self.source.offset_of(start.line, start.column)
    }

    fn loop_at(
        &mut self,
        keyword: proc_macro2::Span,
        node: &impl Spanned,
        header_end: Option<usize>,
    ) {
        let extent = super::range(self.source, node);
        let keyword = self.at(keyword);
        self.out
            .loops
            .push((keyword, Parsed { extent, header_end }));
    }
}

impl<'ast> Visit<'ast> for Walk<'_> {
    fn visit_expr_loop(&mut self, e: &'ast syn::ExprLoop) {
        self.loop_at(e.loop_token.span, e, None);
        visit::visit_expr_loop(self, e);
    }
    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        self.loop_at(e.while_token.span, e, None);
        visit::visit_expr_while(self, e);
    }
    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        let header_end = self.at(e.body.brace_token.span.open());
        self.loop_at(e.for_token.span, e, Some(header_end));
        visit::visit_expr_for_loop(self, e);
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.out.fns.push(super::range(self.source, item));
        visit::visit_item_fn(self, item);
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.out.fns.push(super::range(self.source, item));
        visit::visit_impl_item_fn(self, item);
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.out.fns.push(super::range(self.source, item));
        visit::visit_trait_item_fn(self, item);
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.out.opaque.push(super::range(self.source, mac));
        visit::visit_macro(self, mac);
    }
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        self.out.opaque.push(super::range(self.source, attr));
        visit::visit_attribute(self, attr);
    }
}

/// Every loop with a probe in its own tokens and no marker, as (line of
/// the keyword, keyword, first probe).
pub fn unmarked(source: &Source) -> Result<Vec<(usize, String, String)>, String> {
    let file = syn::parse_file(&source.text).map_err(|e| e.to_string())?;
    let mut walk = Walk {
        source,
        out: Structure::default(),
    };
    walk.visit_file(&file);
    let structure = walk.out;

    let code = source.code();
    let marked = marked_lines(source);
    let probes = probes(source, &code);
    let mut out = Vec::new();
    for tok in &code {
        let Some(keyword) = ["loop", "while", "for"]
            .into_iter()
            .find(|k| source.is_word(tok, k))
        else {
            continue;
        };
        if marked.contains(&tok.line) {
            continue;
        }
        let parsed = structure
            .loops
            .iter()
            .find(|(at, _)| *at == tok.start)
            .map(|(_, p)| p);
        let (extent, header_end) = match parsed {
            Some(p) => (p.extent.clone(), p.header_end),
            None => {
                // Outermost opaque region holding the keyword, if any.
                let Some(region) = structure
                    .opaque
                    .iter()
                    .filter(|r| r.contains(&tok.start))
                    .max_by_key(|r| r.len())
                else {
                    continue;
                };
                (region.clone(), None)
            }
        };
        let traversal = header_end.is_some_and(|end| {
            let header: Vec<Tok> = code
                .iter()
                .copied()
                .filter(|t| t.start > tok.start && t.start < end)
                .collect();
            !spells_endless_source(source, &header)
        });
        let hit = probes.iter().find(|(at, probe)| {
            extent.contains(at)
                && !(traversal && probe == ".load(")
                && !structure
                    .fns
                    .iter()
                    .any(|f| f.start > tok.start && extent.contains(&f.start) && f.contains(at))
        });
        if let Some((_, probe)) = hit {
            out.push((tok.line, keyword.to_owned(), probe.clone()));
        }
    }
    Ok(out)
}

/// Every probe, by byte offset, with its name.
fn probes(source: &Source, code: &[Tok]) -> Vec<(usize, String)> {
    let is = |j: usize, p: &str| code.get(j).is_some_and(|t| source.is_punct(t, p));
    let mut out = Vec::new();
    for (i, tok) in code.iter().enumerate() {
        let Some(name) = source.name(tok) else {
            continue;
        };
        if PROBES.contains(&name) {
            out.push((tok.start, name.to_owned()));
        } else if name == "load" && i > 0 && is(i - 1, ".") && is(i + 1, "(") {
            out.push((tok.start, ".load(".to_owned()));
        }
    }
    out
}

/// Whether a `for` header spells a source that may not end: an
/// [`ENDLESS`] name or a range without an upper bound (`0..`). This
/// recognizes spellings only; it does not establish that any other header
/// ends, and a hand-written endless iterator passes it.
fn spells_endless_source(source: &Source, header: &[Tok]) -> bool {
    let named_endless = header
        .iter()
        .any(|t| source.name(t).is_some_and(|n| ENDLESS.contains(&n)));
    let open_range = (0..header.len().saturating_sub(2)).any(|i| {
        source.is_punct(&header[i], ".")
            && source.is_punct(&header[i + 1], ".")
            && ([")", ",", "}"]
                .into_iter()
                .any(|p| source.is_punct(&header[i + 2], p))
                || source.is_punct(&header[i + 2], "]") && !indexes(source, &header[..i]))
    }) || header.len() >= 2
        && source.is_punct(&header[header.len() - 1], ".")
        && source.is_punct(&header[header.len() - 2], ".");
    named_endless || open_range
}

/// Whether the bracket still open at the end of `before` indexes (`v[1..]`,
/// `a[..]`, a slice) rather than starting an array (`[0..]`, an element).
/// It indexes when the token before it ends an expression.
fn indexes(source: &Source, before: &[Tok]) -> bool {
    let mut depth = 0_usize;
    for (j, tok) in before.iter().enumerate().rev() {
        if source.is_punct(tok, "]") {
            depth += 1;
        } else if source.is_punct(tok, "[") {
            if depth == 0 {
                return j > 0 && {
                    let prev = &before[j - 1];
                    matches!(prev.kind, Kind::Ident | Kind::RawIdent) && !source.is_word(prev, "in")
                        || [")", "]", "?"]
                            .into_iter()
                            .any(|p| source.is_punct(prev, p))
                };
            }
            depth -= 1;
        }
    }
    false
}

fn is_marker(comment: &str) -> bool {
    comment
        .strip_prefix("//")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix(MARKER))
        .is_some_and(|reason| !reason.trim_start().is_empty())
}

/// Lines a marker covers: its own, and when it stands alone, the first line
/// after it and the line comments that continue it.
fn marked_lines(source: &Source) -> BTreeSet<usize> {
    let mut code_lines = BTreeSet::new();
    for tok in source.code() {
        code_lines.extend(tok.line..=tok.end_line);
    }
    let comment_lines: BTreeSet<usize> = source
        .toks
        .iter()
        .filter(|t| t.kind == Kind::LineComment && !code_lines.contains(&t.line))
        .map(|t| t.line)
        .collect();
    let mut marked = BTreeSet::new();
    for tok in &source.toks {
        if tok.kind == Kind::LineComment && is_marker(source.text_of(tok)) {
            marked.insert(tok.line);
            if !code_lines.contains(&tok.line) {
                let mut next = tok.line + 1;
                while comment_lines.contains(&next) {
                    next += 1;
                }
                marked.insert(next);
            }
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::unmarked;
    use crate::lex::Source;

    fn found(src: &str) -> Vec<(String, String)> {
        unmarked(&Source::new(src.to_owned()))
            .expect("parses")
            .into_iter()
            .map(|(_, keyword, probe)| (keyword, probe))
            .collect()
    }

    fn lines(src: &str) -> Vec<usize> {
        unmarked(&Source::new(src.to_owned()))
            .expect("parses")
            .into_iter()
            .map(|(line, _, _)| line)
            .collect()
    }

    #[test]
    fn probing_loops_need_a_reason() {
        for (src, keyword, probe) in [
            (
                "fn f() { loop { if rx.try_recv().is_ok() { break; } } }",
                "loop",
                "try_recv",
            ),
            ("fn f() { while !done.load(SeqCst) {} }", "while", ".load("),
            (
                "fn f() { while !stopped() { edge.load(Acquire); } }",
                "while",
                ".load(",
            ),
            (
                "fn f() { while Instant::now() < deadline {} }",
                "while",
                "Instant",
            ),
            (
                "fn f() { for _ in 0..n { m.try_lock(); } }",
                "for",
                "try_lock",
            ),
            (
                "fn f() { 'a: loop { let c = || x.load(o); }; }",
                "loop",
                ".load(",
            ),
            ("fn f() { let v = loop { x.load(o); }; }", "loop", ".load("),
            (
                "fn f() { while { k() } { rx.try_recv(); } }",
                "while",
                "try_recv",
            ),
            (
                "fn f() { while { k() } != 0 { rx.try_recv(); } }",
                "while",
                "try_recv",
            ),
            (
                "fn f() { while let Some(x) = { n() } { rx.try_recv(); } }",
                "while",
                "try_recv",
            ),
            (
                "fn f() { for x in { it() } { rx.try_recv(); } }",
                "for",
                "try_recv",
            ),
        ] {
            assert_eq!(
                found(src),
                [(keyword.to_owned(), probe.to_owned())],
                "{src}"
            );
        }
    }

    #[test]
    fn endless_sources_and_opaque_shapes_keep_the_obligation() {
        for (src, keyword, probe) in [
            (
                "fn f() { for _ in 0.. { if f.load(o) { break; } } }",
                "for",
                ".load(",
            ),
            (
                "fn f() { for _ in { 0.. } { f.load(o); } }",
                "for",
                ".load(",
            ),
            (
                "fn f() { for _ in (0..).take(n) { f.load(o); } }",
                "for",
                ".load(",
            ),
            (
                "fn f() { for _ in std::iter::repeat(()) { f.load(o); } }",
                "for",
                ".load(",
            ),
            (
                "fn f() { loop { let c: fn() = || { F.load(o); }; c(); } }",
                "loop",
                ".load(",
            ),
            (
                "fn f() { while match s { A => true, _ => false } { rx.try_recv(); } }",
                "while",
                "try_recv",
            ),
            (
                "fn f() { for x in if c { a } else { b } { rx.try_recv(); } }",
                "for",
                "try_recv",
            ),
            ("fn f() { while !d.load(o) {} &value; }", "while", ".load("),
            (
                "fn f() { for _ in [0..].into_iter().flatten() { f.load(o); } }",
                "for",
                ".load(",
            ),
            (
                "fn f() {\n    // loop-ends: a reason\n\n    loop { rx.try_recv(); }\n}",
                "loop",
                "try_recv",
            ),
            (
                "macro_rules! m { ({$k:tt} $b:block) => { $k $b }; }\n\
                 fn f() { m!({loop} { F.load(o); }); }",
                "loop",
                ".load(",
            ),
            (
                "fn f() {\n    // loop-ends:\n    loop { rx.try_recv(); }\n}",
                "loop",
                "try_recv",
            ),
        ] {
            assert_eq!(
                found(src),
                [(keyword.to_owned(), probe.to_owned())],
                "{src}"
            );
        }
    }

    #[test]
    fn every_enclosing_loop_is_obliged() {
        let src = "fn f() {\n    loop {\n        // loop-ends: drains, breaks on Empty\n        \
                   while let Ok(x) = rx.try_recv() { use_it(x); }\n    }\n}";
        assert_eq!(found(src), [("loop".to_owned(), "try_recv".to_owned())]);
    }

    #[test]
    fn a_nested_fn_obliges_only_its_own_loops() {
        let src = "fn f() {\n    loop {\n        fn stop() -> ! {\n            \
                   loop { F.load(o); }\n        }\n    }\n}";
        assert_eq!(lines(src), [4]);
    }

    #[test]
    fn an_unparsable_file_is_an_error() {
        assert!(unmarked(&Source::new("fn f( {".to_owned())).is_err());
    }

    #[test]
    fn marked_and_probe_free_loops_pass() {
        for src in [
            "fn f() { loop { rx.try_recv(); } } // loop-ends: a reason",
            "fn f() {\n    // loop-ends: a reason\n    loop { rx.try_recv(); }\n}",
            "fn f() {\n    // loop-ends: a reason that\n    // wraps\n    loop { rx.try_recv(); }\n}",
            "fn f() { loop { rx.recv(); } }",
            "fn f() { for x in xs { total += x; } }",
            "fn f() { let v = done.load(SeqCst); rx.try_recv(); }",
            "impl Iterator for It { fn next(&mut self) { self.rx.try_recv(); } }",
            "fn f() where for<'a> F: Fn(&'a u8) { rx.try_recv(); }",
            "fn f() { loop { fn g() { rx.try_recv(); } g(); } }",
            "fn f() { loop { let s = \"rx.try_recv()\"; } }",
            "fn f() { loop { x.unload(); y.load_all(); } }",
            "fn f() { for w in &words { n += w.load(Relaxed); } }",
            "fn f() { for i in 0..=n { n += w[i].load(Relaxed); } }",
            "fn f() { for r in [&a[..], &b[1..]] { n += r[0].load(Relaxed); } }",
            "fn f() -> Vec<u8> { rx.try_recv(); vec![] }",
            "fn f() { assert!(rx.try_recv().is_err(), \"for loop while\"); }",
        ] {
            assert!(found(src).is_empty(), "{src}");
        }
    }
}
