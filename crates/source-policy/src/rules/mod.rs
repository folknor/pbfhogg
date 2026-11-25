//! One module per rule. Each exposes `errors(&Corpus) -> Vec<String>` and
//! carries its own in-memory fixtures.

pub mod any_error;
pub mod counters;
pub mod detached_threads;
pub mod duration_literals;
pub mod entropy;
pub mod environment;
pub mod host_independence;
pub mod id_signs;
pub mod io_sizes;
pub mod library_text;
pub mod lifecycle;
pub mod loop_waits;
pub mod statics;
pub mod test_imports;
pub mod test_modules;
pub mod timed_waits;

use std::collections::BTreeSet;
use std::ops::Range;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::Corpus;
use crate::lex::{Kind, Source};

/// Each named item's byte range with its path, from syn's item tree.
/// Owners are `mod` (inline), `fn`, `trait` names and an `impl`'s self
/// type (its last path segment), joined by `::`.
pub(crate) struct Owners {
    spans: Vec<(Range<usize>, String)>,
}

impl Owners {
    pub(crate) fn of(source: &Source, file: &syn::File) -> Self {
        let mut walk = Walk {
            source,
            stack: Vec::new(),
            spans: Vec::new(),
        };
        walk.visit_file(file);
        Self { spans: walk.spans }
    }

    /// The path of the innermost named item holding byte `offset`.
    pub(crate) fn owner(&self, offset: usize) -> &str {
        self.spans
            .iter()
            .filter(|(r, _)| r.contains(&offset))
            .max_by_key(|(r, _)| r.start)
            .map_or("", |(_, p)| p.as_str())
    }

    /// How many named items carry `path`: more than one (two impls of one
    /// self type, say) means the path does not identify a declaration.
    pub(crate) fn declarations(&self, path: &str) -> usize {
        self.spans.iter().filter(|(_, p)| p == path).count()
    }
}

/// A syn node's byte range in `source`.
pub(crate) fn range(source: &Source, node: &impl Spanned) -> Range<usize> {
    let span = node.span();
    let (start, end) = (span.start(), span.end());
    source.offset_of(start.line, start.column)..source.offset_of(end.line, end.column)
}

struct Walk<'s> {
    source: &'s Source,
    stack: Vec<String>,
    spans: Vec<(Range<usize>, String)>,
}

impl Walk<'_> {
    fn enter(&mut self, name: String, node: &impl Spanned, inner: impl FnOnce(&mut Self)) {
        self.stack.push(name);
        self.spans
            .push((range(self.source, node), self.stack.join("::")));
        inner(self);
        self.stack.pop();
    }
}

impl<'ast> Visit<'ast> for Walk<'_> {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.enter(item.sig.ident.to_string(), item, |w| {
            visit::visit_item_fn(w, item);
        });
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.enter(item.sig.ident.to_string(), item, |w| {
            visit::visit_impl_item_fn(w, item);
        });
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.enter(item.sig.ident.to_string(), item, |w| {
            visit::visit_trait_item_fn(w, item);
        });
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.enter(item.ident.to_string(), item, |w| {
            visit::visit_item_mod(w, item);
        });
    }
    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.enter(item.ident.to_string(), item, |w| {
            visit::visit_item_trait(w, item);
        });
    }
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let name = match &*item.self_ty {
            syn::Type::Path(p) => p
                .path
                .segments
                .last()
                .map_or_else(String::new, |s| s.ident.to_string()),
            _ => "_".to_owned(),
        };
        self.enter(name, item, |w| visit::visit_item_impl(w, item));
    }
}

/// The prefix selects at least one file; a layout change that empties it
/// must fail rather than audit nothing.
pub(crate) fn require_files(corpus: &Corpus, prefix: &str, rule: &str) -> Option<String> {
    corpus
        .under(prefix)
        .next()
        .is_none()
        .then(|| format!("{rule}: no Rust file under {prefix}"))
}

/// Whether a line comment is `// <marker> <reason>` with a reason present.
fn is_marker(comment: &str, marker: &str) -> bool {
    comment
        .strip_prefix("//")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix(marker))
        .is_some_and(|reason| !reason.trim_start().is_empty())
}

/// Lines a `// <marker> <reason>` comment covers: its own, and the next
/// when it stands alone on its line. A marker's reason is checked only for
/// being present.
pub(crate) fn marked_lines(source: &Source, marker: &str) -> BTreeSet<usize> {
    let mut code_lines = BTreeSet::new();
    for tok in source.code() {
        code_lines.extend(tok.line..=tok.end_line);
    }
    let mut marked = BTreeSet::new();
    for tok in &source.toks {
        if tok.kind == Kind::LineComment && is_marker(source.text_of(tok), marker) {
            marked.insert(tok.line);
            if !code_lines.contains(&tok.line) {
                marked.insert(tok.line + 1);
            }
        }
    }
    marked
}

/// The listed files are present in the corpus.
pub(crate) fn require_sentinels(corpus: &Corpus, paths: &[&str], rule: &str) -> Vec<String> {
    paths
        .iter()
        .filter(|path| !corpus.files.contains_key(**path))
        .map(|path| format!("{rule}: corpus sentinel missing: {path}"))
        .collect()
}
