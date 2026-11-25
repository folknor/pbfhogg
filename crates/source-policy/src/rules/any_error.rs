//! A test that expects a failure names the cause it promises.
//!
//! An assertion that accepts any error passes on the wrong refusal and on a
//! worker's caught panic (`ErrorKind::Panicked`) alike, so a defect the test
//! exists to catch reads as a pass. In test code this rule flags the shapes
//! that accept or discard an arbitrary error: `.is_err()`, `!x.is_ok()`,
//! `.err().is_some()`, `.is_err_and(..)`, `matches!(_, Err(_))`, an
//! `Err(_)` match arm or `if let Err(_) = ..` body that does not fail,
//! `let Err(_) = .. else`, and an `unwrap_err`/`expect_err` whose value is
//! discarded (a bare statement, `let _`, a `_`-prefixed binding, `drop`).
//! `Err(_)` means `Err(_)`, `Err(..)` or `Err(_name)`, under any path
//! (`Result::Err`) and in an or-pattern. A body fails when it holds, as a
//! statement or its value, `panic!`, `unreachable!`, `todo!`,
//! `unimplemented!`, `panic_any`, `resume_unwind` or `return Err(..)`: a
//! test-local `Err(_) => panic!(..)` makes an unexpected outcome fail and is
//! legal.
//!
//! The bar instead: an expected kind or a closed set of kinds, plus detail
//! where the kind is broad. Where a contract is genuinely cause-agnostic (a
//! filesystem probe, a join guard, a deliberate helper panic caught by
//! `catch_unwind`) the site carries `// any-error-deliberate: <reason>`. The
//! marker binds to a syntax node, not a line: it exempts a finding when it
//! directly precedes (only whitespace and comments between) or directly
//! trails on the same line (after an optional `;` or `,`) the flagged
//! expression or any node enclosing it up to its innermost statement or
//! match arm. A marker that exempts nothing is a finding, and marker text
//! inside a string is not a comment.
//!
//! Test code is every Rust file under a member's `tests/`, the test-only
//! regions of every member's `src/` (the gates the library text rule
//! recognises), and every `src/` file reached through a `mod` declaration
//! inside such a region or inside another test-only file (`#[path]` read
//! relative to the declaring file's directory, inline modules adding their
//! names). Macro input is read only where it parses: `matches!` as an
//! expression, a pattern and an optional guard, any other macro as
//! comma-separated expressions (the assertion macros); input of another
//! shape (`macro_rules!` bodies, `vec![x; n]`) stays opaque. A review
//! backstop, not proof: it has no types, so a `Result` checked through a
//! helper or a method of another name passes it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;

use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Expr, Pat, Stmt};

use crate::Corpus;
use crate::lex::{Kind, Source};

const MARKER: &str = "any-error-deliberate:";

/// The integration test trees named by the design: each must be present.
const SENTINELS: &[&str] = &[
    "crates/lib/tests/all.rs",
    "crates/cli/tests/all.rs",
    "crates/harness/tests/all.rs",
];

/// Identifiers without which a file holds no candidate.
const CANDIDATE_NAMES: &[&str] = &[
    "is_err",
    "is_ok",
    "err",
    "is_err_and",
    "Err",
    "unwrap_err",
    "expect_err",
];

const PANIC_MACROS: &[&str] = &["panic", "unreachable", "todo", "unimplemented"];
const PANIC_CALLS: &[&str] = &["panic_any", "resume_unwind"];

/// Which bytes of a file are test code.
enum Scope {
    Whole,
    Ranges(Vec<Range<usize>>),
}

impl Scope {
    fn holds(&self, offset: usize) -> bool {
        match self {
            Self::Whole => true,
            Self::Ranges(ranges) => ranges.iter().any(|r| r.contains(&offset)),
        }
    }
}

/// Whether `path` lies in a member's `tests/` tree.
fn in_test_tree(path: &str) -> bool {
    path.strip_prefix("crates/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, rest)| rest.starts_with("tests/"))
}

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found = super::require_sentinels(corpus, SENTINELS, "any error");
    let mut scopes: BTreeMap<&str, Scope> = BTreeMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for (path, source) in &corpus.files {
        if in_test_tree(path) {
            scopes.insert(path.as_str(), Scope::Whole);
            continue;
        }
        if !path.contains("/src/") || !mentions(source, &["test"]) {
            continue;
        }
        let file = match syn::parse_file(&source.text) {
            Ok(file) => file,
            Err(e) => {
                found.push(format!(
                    "{path}: does not parse ({e}), so its test code cannot be audited for \
                     any-error assertions"
                ));
                continue;
            }
        };
        let gated = super::library_text::test_gated(source, &file);
        if gated.is_empty() {
            continue;
        }
        for (offset, targets) in mod_decls(path, source, &file) {
            if gated.iter().any(|r| r.contains(&offset)) {
                queue.extend(targets);
            }
        }
        scopes.insert(path.as_str(), Scope::Ranges(gated));
    }
    // Files mounted from test-only code, transitively.
    let mut mounted = BTreeSet::new();
    while let Some(candidate) = queue.pop_front() {
        let Some((path, source)) = corpus.files.get_key_value(&candidate) else {
            continue;
        };
        if in_test_tree(path) || !mounted.insert(path.as_str()) {
            continue;
        }
        scopes.insert(path.as_str(), Scope::Whole);
        if let Ok(file) = syn::parse_file(&source.text) {
            for (_, targets) in mod_decls(path, source, &file) {
                queue.extend(targets);
            }
        }
    }
    for (path, scope) in &scopes {
        let source = &corpus.files[*path];
        if !mentions(source, CANDIDATE_NAMES) && markers(source).is_empty() {
            continue;
        }
        match audit_file(source, scope) {
            Ok(lines) => found.extend(lines.into_iter().map(|l| format!("{path}:{l}"))),
            Err(e) => found.push(format!(
                "{path}: does not parse ({e}), so its test code cannot be audited for \
                 any-error assertions"
            )),
        }
    }
    found
}

/// Whether any code identifier of `source` is one of `names`.
fn mentions(source: &Source, names: &[&str]) -> bool {
    source
        .toks
        .iter()
        .any(|t| source.name(t).is_some_and(|n| names.contains(&n)))
}

/// The token indices of every marker comment in `source`.
fn markers(source: &Source) -> Vec<usize> {
    (0..source.toks.len())
        .filter(|&i| {
            let tok = &source.toks[i];
            tok.kind == Kind::LineComment && super::is_marker(source.text_of(tok), MARKER)
        })
        .collect()
}

/// Every finding in one file, as `line: message`.
fn audit_file(source: &Source, scope: &Scope) -> Result<Vec<String>, String> {
    let file = syn::parse_file(&source.text)
        .map_err(|e| format!("line {}: {e}", e.span().start().line))?;
    let mut scan = Scan {
        source,
        stack: Vec::new(),
        hits: Vec::new(),
    };
    scan.visit_file(&file);
    let all_markers = markers(source);
    let mut used = BTreeSet::new();
    let mut out = Vec::new();
    for hit in scan.hits {
        if !scope.holds(hit.offset) {
            continue;
        }
        let binding: Vec<usize> = hit
            .anchors
            .iter()
            .filter_map(|r| attached(source, r, &all_markers))
            .collect();
        if binding.is_empty() {
            out.push(format!(
                "{}: {} accepts any error; assert the promised kind or mark \
                 `// {MARKER} <reason>` (rules/any_error.rs)",
                source.line_of(hit.offset),
                hit.form
            ));
        }
        used.extend(binding);
    }
    for i in all_markers {
        if !used.contains(&i) {
            out.push(format!(
                "{}: `{MARKER}` marker exempts no any-error finding; remove it",
                source.toks[i].line
            ));
        }
    }
    Ok(out)
}

/// The marker token attached to the node spanning `range`: directly before
/// it (only whitespace and comments between) or trailing it on its line.
fn attached(source: &Source, range: &Range<usize>, marker_toks: &[usize]) -> Option<usize> {
    let toks = &source.toks;
    let first = toks.partition_point(|t| t.start < range.start);
    let mut i = first;
    while i > 0 {
        i -= 1;
        match toks[i].kind {
            Kind::Whitespace | Kind::BlockComment => {}
            Kind::LineComment => {
                if marker_toks.contains(&i) {
                    return Some(i);
                }
            }
            _ => break,
        }
    }
    let mut j = toks.partition_point(|t| t.end <= range.end.saturating_sub(1).max(range.start));
    // `j` is the token holding the node's last byte; read past it.
    j += 1;
    let mut punct_seen = false;
    while let Some(tok) = toks.get(j) {
        match tok.kind {
            Kind::Whitespace if !source.text_of(tok).contains('\n') => {}
            Kind::Punct
                if !punct_seen && (source.is_punct(tok, ";") || source.is_punct(tok, ",")) =>
            {
                punct_seen = true;
            }
            Kind::LineComment => return marker_toks.contains(&j).then_some(j),
            _ => return None,
        }
        j += 1;
    }
    None
}

/// Each `mod name;` declaration in `file`: its offset and the corpus paths
/// it may resolve to.
fn mod_decls(path: &str, source: &Source, file: &syn::File) -> Vec<(usize, Vec<String>)> {
    let (parent, stem) = match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name.trim_end_matches(".rs")),
        None => ("", path.trim_end_matches(".rs")),
    };
    let own = if matches!(stem, "mod" | "lib" | "main") {
        parent.to_owned()
    } else {
        format!("{parent}/{stem}")
    };
    let mut out = Vec::new();
    collect_decls(source, &file.items, parent, &own, &mut out);
    out
}

fn collect_decls(
    source: &Source,
    items: &[syn::Item],
    path_dir: &str,
    dir: &str,
    out: &mut Vec<(usize, Vec<String>)>,
) {
    for item in items {
        let syn::Item::Mod(m) = item else {
            continue;
        };
        let name = m.ident.to_string();
        if let Some((_, inner)) = &m.content {
            let nested = format!("{dir}/{name}");
            collect_decls(source, inner, &nested, &nested, out);
            continue;
        }
        let explicit = m.attrs.iter().find_map(|a| match &a.meta {
            syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
                Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        });
        let targets = match explicit {
            Some(rel) => vec![lexical(&format!("{path_dir}/{rel}"))],
            None => vec![format!("{dir}/{name}.rs"), format!("{dir}/{name}/mod.rs")],
        };
        out.push((super::range(source, m).start, targets));
    }
}

/// `path` with `.` and `..` segments resolved.
fn lexical(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// One flagged shape: where, what, and the nodes a marker may attach to.
struct Hit {
    offset: usize,
    form: &'static str,
    anchors: Vec<Range<usize>>,
}

struct Scan<'a> {
    source: &'a Source,
    /// Enclosing nodes, innermost last; `true` marks a statement or arm.
    stack: Vec<(Range<usize>, bool)>,
    hits: Vec<Hit>,
}

impl Scan<'_> {
    fn hit(&mut self, node: &impl Spanned, form: &'static str) {
        let own = super::range(self.source, node);
        let from = self.stack.iter().rposition(|(_, b)| *b).unwrap_or(0);
        let mut anchors: Vec<Range<usize>> =
            self.stack[from..].iter().map(|(r, _)| r.clone()).collect();
        anchors.push(own.clone());
        self.hits.push(Hit {
            offset: own.start,
            form,
            anchors,
        });
    }

    fn within(&mut self, node: &impl Spanned, boundary: bool, inner: impl FnOnce(&mut Self)) {
        self.stack.push((super::range(self.source, node), boundary));
        inner(self);
        self.stack.pop();
    }
}

fn strip(mut e: &Expr) -> &Expr {
    while let Expr::Paren(p) = e {
        e = &p.expr;
    }
    e
}

fn method_named<'e>(e: &'e Expr, name: &str) -> Option<&'e syn::ExprMethodCall> {
    match strip(e) {
        Expr::MethodCall(m) if m.method == name => Some(m),
        _ => None,
    }
}

fn is_unwrap_err(e: &Expr) -> bool {
    method_named(e, "unwrap_err").is_some() || method_named(e, "expect_err").is_some()
}

/// A pattern that binds nothing it keeps: `_`, `..`, `_name`.
fn discards(p: &Pat) -> bool {
    match p {
        Pat::Wild(_) | Pat::Rest(_) => true,
        Pat::Ident(i) => i.subpat.is_none() && i.ident.to_string().starts_with('_'),
        Pat::Type(t) => discards(&t.pat),
        Pat::Paren(p) => discards(&p.pat),
        _ => false,
    }
}

/// `Err(_)`, `Err(..)`, `Err(_name)`, under any path or in an or-pattern.
fn err_wild(p: &Pat) -> bool {
    match p {
        Pat::TupleStruct(t) => {
            t.path.segments.last().is_some_and(|s| s.ident == "Err")
                && t.elems.len() == 1
                && t.elems.first().is_some_and(discards)
        }
        Pat::Or(o) => o.cases.iter().any(err_wild),
        Pat::Paren(p) => err_wild(&p.pat),
        _ => false,
    }
}

fn macro_name(mac: &syn::Macro) -> String {
    mac.path
        .segments
        .last()
        .map_or_else(String::new, |s| s.ident.to_string())
}

fn is_panic_macro(mac: &syn::Macro) -> bool {
    PANIC_MACROS.contains(&macro_name(mac).as_str())
}

/// Whether evaluating `e` fails the test.
fn fails(e: &Expr) -> bool {
    match strip(e) {
        Expr::Macro(m) => is_panic_macro(&m.mac),
        Expr::Block(b) => block_fails(&b.block),
        Expr::Return(r) => r.expr.as_deref().is_some_and(|v| {
            matches!(strip(v), Expr::Call(c) if matches!(strip(&c.func),
                Expr::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Err")))
        }),
        Expr::Call(c) => matches!(strip(&c.func), Expr::Path(p)
            if p.path.segments.last().is_some_and(|s| PANIC_CALLS.contains(&s.ident.to_string().as_str()))),
        _ => false,
    }
}

fn block_fails(b: &syn::Block) -> bool {
    b.stmts.iter().any(|s| match s {
        Stmt::Macro(m) => is_panic_macro(&m.mac),
        Stmt::Expr(e, _) => fails(e),
        _ => false,
    })
}

/// `matches!` input: the scrutinee, the pattern and an optional guard.
fn parse_matches(input: syn::parse::ParseStream) -> syn::Result<(Expr, Pat, Option<Expr>)> {
    let scrutinee: Expr = input.parse()?;
    input.parse::<syn::Token![,]>()?;
    let pat = Pat::parse_multi_with_leading_vert(input)?;
    let guard = if input.peek(syn::Token![if]) {
        input.parse::<syn::Token![if]>()?;
        Some(input.parse()?)
    } else {
        None
    };
    if input.peek(syn::Token![,]) {
        input.parse::<syn::Token![,]>()?;
    }
    Ok((scrutinee, pat, guard))
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        self.within(stmt, true, |s| {
            match stmt {
                Stmt::Expr(e, Some(_)) if is_unwrap_err(e) => {
                    s.hit(e, "a discarded `unwrap_err`/`expect_err`");
                }
                Stmt::Local(local) => {
                    if let Some(init) = &local.init {
                        if discards(&local.pat) && is_unwrap_err(&init.expr) {
                            s.hit(local, "a discarded `unwrap_err`/`expect_err`");
                        } else if err_wild(&local.pat) {
                            s.hit(local, "`let Err(_) = .. else`");
                        }
                    }
                }
                _ => {}
            }
            visit::visit_stmt(s, stmt);
        });
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        self.within(arm, true, |s| {
            if err_wild(&arm.pat) && !fails(&arm.body) {
                s.hit(&arm.pat, "an `Err(_)` arm that does not fail");
            }
            visit::visit_arm(s, arm);
        });
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        self.within(expr, false, |s| {
            match expr {
                Expr::MethodCall(m) if m.method == "is_err" && m.args.is_empty() => {
                    s.hit(expr, "`.is_err()`");
                }
                Expr::MethodCall(m) if m.method == "is_err_and" => {
                    s.hit(expr, "`.is_err_and(..)`");
                }
                Expr::MethodCall(m)
                    if m.method == "is_some"
                        && m.args.is_empty()
                        && method_named(&m.receiver, "err").is_some_and(|r| r.args.is_empty()) =>
                {
                    s.hit(expr, "`.err().is_some()`");
                }
                Expr::Unary(u)
                    if matches!(u.op, syn::UnOp::Not(_))
                        && method_named(&u.expr, "is_ok").is_some_and(|m| m.args.is_empty()) =>
                {
                    s.hit(expr, "`!x.is_ok()`");
                }
                Expr::Call(c)
                    if matches!(strip(&c.func), Expr::Path(p) if p.path.is_ident("drop"))
                        && c.args.len() == 1
                        && c.args.first().is_some_and(is_unwrap_err) =>
                {
                    s.hit(expr, "a discarded `unwrap_err`/`expect_err`");
                }
                Expr::If(i) => {
                    if let Expr::Let(l) = strip(&i.cond)
                        && err_wild(&l.pat)
                        && !block_fails(&i.then_branch)
                    {
                        s.hit(&l.pat, "an `if let Err(_)` body that does not fail");
                    }
                }
                _ => {}
            }
            visit::visit_expr(s, expr);
        });
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = macro_name(mac);
        if name == "macro_rules" {
            return;
        }
        if name == "matches" {
            if let Ok((scrutinee, pat, guard)) =
                syn::parse::Parser::parse2(parse_matches, mac.tokens.clone())
            {
                if err_wild(&pat) {
                    self.hit(mac, "`matches!(_, Err(_))`");
                }
                self.visit_expr(&scrutinee);
                if let Some(guard) = &guard {
                    self.visit_expr(guard);
                }
            }
            return;
        }
        if let Ok(args) = syn::parse::Parser::parse2(
            Punctuated::<Expr, syn::Token![,]>::parse_terminated,
            mac.tokens.clone(),
        ) {
            for arg in &args {
                self.visit_expr(arg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::errors;
    use crate::Corpus;

    const FILE: &str = "crates/lib/tests/a.rs";

    fn corpus_with(extra: &[(&'static str, &'static str)]) -> Corpus {
        let mut files: Vec<(&str, &str)> = vec![
            ("crates/lib/tests/all.rs", "mod a;"),
            ("crates/cli/tests/all.rs", ""),
            ("crates/harness/tests/all.rs", ""),
        ];
        files.extend_from_slice(extra);
        Corpus::from_sources(files)
    }

    fn found(src: &'static str) -> Vec<String> {
        errors(&corpus_with(&[(FILE, src)]))
    }

    #[test]
    fn every_any_error_form_is_found() {
        for src in [
            "#[test] fn t() { assert!(f().is_err()); }",
            "#[test] fn t() { assert!(!f().is_ok()); }",
            "#[test] fn t() { assert!(f().err().is_some()); }",
            "#[test] fn t() { assert!(f().is_err_and(|e| e.is_io())); }",
            "#[test] fn t() { assert!(matches!(f(), Err(_))); }",
            "#[test] fn t() { assert!(matches!(f(), Ok(1) | Err(..))); }",
            "#[test] fn t() { match f() { Ok(_) => panic!(), Err(_) => {} } }",
            "#[test] fn t() { match f() { Ok(_) => panic!(), Result::Err(_e) => () } }",
            "#[test] fn t() { if let Err(_) = f() { seen(); } }",
            "#[test] fn t() { let Err(_) = f() else { panic!() }; }",
            "#[test] fn t() { f().unwrap_err(); }",
            "#[test] fn t() { f().expect_err(\"x\"); }",
            "#[test] fn t() { let _ = f().unwrap_err(); }",
            "#[test] fn t() { let _e = f().unwrap_err(); }",
            "#[test] fn t() { drop(f().unwrap_err()); }",
            // A newline between the call and its parenthesis is one call.
            "#[test] fn t() { f()\n    .expect_err\n    (\"x\");\n}",
            "#[test] fn t() { assert_eq!(f().is_err(), true, \"{}\", 1); }",
        ] {
            let got = found(src);
            assert_eq!(got.len(), 1, "{src}: {got:?}");
            assert!(got[0].starts_with("crates/lib/tests/a.rs:"), "{got:?}");
        }
    }

    #[test]
    fn named_causes_and_failing_arms_are_legal() {
        for src in [
            "#[test] fn t() { let e = f().unwrap_err(); assert!(e.is_cancelled()); }",
            "#[test] fn t() { match f() { Err(e) => check(e), Ok(_) => panic!() } }",
            "#[test] fn t() { match f() { Ok(v) => use_it(v), Err(_) => panic!(\"unexpected\") } }",
            "#[test] fn t() { match f() { Ok(v) => v, Err(_) => unreachable!() }; }",
            "#[test] fn t() { match f() { Ok(v) => v, Err(_) => { log(); panic!() } }; }",
            "fn h() -> Result<(), String> { match f() { Ok(v) => Ok(v), Err(_) => return Err(s()) } }",
            "#[test] fn t() { if let Err(_) = f() { panic!(\"must succeed\"); } }",
            "#[test] fn t() { assert!(matches!(f(), Err(Error::Gone))); }",
            "#[test] fn t() { assert!(f().is_ok()); }",
            "#[test] fn t() { let s = \".is_err()\"; }",
        ] {
            let got = found(src);
            assert!(got.is_empty(), "{src}: {got:?}");
        }
    }

    #[test]
    fn the_marker_binds_to_a_node_not_a_line() {
        let preceding = "#[test] fn t() {\n    // any-error-deliberate: a probe\n    assert!(probe().is_err());\n}";
        assert!(found(preceding).is_empty(), "{:?}", found(preceding));
        // Two candidates on one line: a trailing marker exempts only the
        // statement it trails.
        let two = "#[test] fn t() {\n    let a = x().is_err(); let b = y().is_err(); // any-error-deliberate: probe\n}";
        let got = found(two);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains(":2:"), "{got:?}");
        // Inside an argument list, a marker binds to the argument it precedes.
        let args = "#[test] fn t() {\n    check(\n        // any-error-deliberate: probe\n        x().is_err(),\n        y().is_err(),\n    );\n}";
        let got = found(args);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains(":5:"), "{got:?}");
        // Marker text in a string exempts nothing.
        let string =
            "#[test] fn t() { let _m = \"// any-error-deliberate: no\"; assert!(x().is_err()); }";
        assert_eq!(found(string).len(), 1);
        // No reason, no marker.
        let bare = "#[test] fn t() {\n    // any-error-deliberate:\n    assert!(x().is_err());\n}";
        assert_eq!(found(bare).len(), 1);
    }

    #[test]
    fn an_unused_marker_is_a_finding() {
        let src =
            "#[test] fn t() {\n    // any-error-deliberate: stale\n    assert!(x().is_ok());\n}";
        let got = found(src);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("exempts no"), "{got:?}");
    }

    #[test]
    fn src_test_regions_and_mounted_files_are_in_scope() {
        let got = errors(&corpus_with(&[
            (
                "crates/lib/src/lib.rs",
                "fn live() -> bool { f().is_err() }\n\
                 #[cfg(test)]\nmod tests { fn t() { assert!(f().is_err()); } }\n\
                 #[cfg(test)]\nmod fixture;\n\
                 mod production;",
            ),
            (
                "crates/lib/src/fixture.rs",
                "mod deeper;\nfn t() { assert!(f().is_err()); }",
            ),
            (
                "crates/lib/src/fixture/deeper.rs",
                "fn t() { assert!(f().is_err()); }",
            ),
            (
                "crates/lib/src/production.rs",
                "fn live() -> bool { f().is_err() }",
            ),
            (
                "crates/cli/src/lib.rs",
                "#[cfg(test)]\n#[path = \"helpers/probe.rs\"]\nmod probe;",
            ),
            (
                "crates/cli/src/helpers/probe.rs",
                "fn t() { assert!(f().is_err()); }",
            ),
            (
                "crates/verify/tests/x.rs",
                "fn t() { assert!(f().is_err()); }",
            ),
        ]));
        let files: Vec<&str> = got
            .iter()
            .map(|l| l.split(':').next().unwrap_or(""))
            .collect();
        assert_eq!(
            files,
            [
                "crates/cli/src/helpers/probe.rs",
                "crates/lib/src/fixture.rs",
                "crates/lib/src/fixture/deeper.rs",
                "crates/lib/src/lib.rs",
                "crates/verify/tests/x.rs",
            ],
            "{got:?}"
        );
        assert!(got[3].starts_with("crates/lib/src/lib.rs:3:"), "{got:?}");
    }

    #[test]
    fn missing_trees_fail() {
        assert_eq!(
            errors(&Corpus::from_sources([("crates/x/src/a.rs", "")])).len(),
            3
        );
    }
}
