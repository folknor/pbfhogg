//! CLI tests name the library only through its fixture and
//! test-infrastructure surface.
//!
//! A CLI test proves what the `pbfhogg` binary does. It may name the library
//! only to build inputs, read outputs and spell the binary's test hooks; any
//! other library item it names couples it to an internal shape, or lets a
//! library call stand in for the binary. Every `pbfhogg::` path spelled in
//! `crates/cli/tests/**/*.rs` outside [`SURFACE`] is refused unless a
//! `// library-deliberate: <reason>` line comment sits on its line, alone on
//! the line above it, or on the first line of its `use` statement.
//!
//! A direct-spelling review guard over tokens, macro input included, and
//! fail-closed on use-tree syntax it does not know. Every `use` statement is
//! read as a tree (nested braces, grouped roots, `self`, globs, renames, raw
//! identifiers, comments between tokens) and its leaves are checked; outside
//! `use` statements every path rooted at `pbfhogg` is checked. A module path
//! (`writer`, `writer::*`, `writer::{self}`) never passes; an item on the
//! surface passes with any further segments (variants, associated items). A
//! rename of the crate and `extern crate pbfhogg` are refused. Outside its
//! promise: re-exports through `crate::`, the helpers in
//! `crates/lib/tests/common/` the CLI binary mounts by path, macro
//! expansion, and whether a test's oracle is independent of the code under
//! test (`reference/testing.md` owns that rule).

use std::collections::BTreeSet;

use crate::Corpus;
use crate::lex::{Kind, Source};

const ROOT: &str = "pbfhogg";
const TREE: &str = "crates/cli/tests/";
const SENTINELS: [&str; 3] = [
    "crates/cli/tests/invoker.rs",
    "crates/cli/tests/cli_sort.rs",
    "crates/cli/tests/cli_flag_contract.rs",
];

/// The one inventory of what a CLI test may name without a marker.
pub const SURFACE: &[&str] = &[
    // Building inputs.
    "block_builder::BlockBuilder",
    "block_builder::HeaderBuilder",
    "block_builder::MemberData",
    "block_builder::Metadata",
    "writer::PbfWriter",
    "writer::Compression",
    // Reading outputs.
    "Blob",
    "BlobDecode",
    "BlobError",
    "BlobReader",
    "BlobType",
    "Element",
    "ElementReader",
    "ErrorKind",
    "HeaderBlock",
    "HeaderOverrides",
    "MemberId",
    "MemberType",
    "Way",
    "Info",
    "DenseNodeInfo",
    "geocode_index::reader::Reader",
    // Test infrastructure: the hook protocol the invoker spells, and the
    // child-timeout, observation and hang bounds.
    "test_hooks::PublishFault",
    "test_hooks::PublishStep",
    "TEST_CHILD_TIMEOUT",
    "TEST_OBSERVE_BOUND",
    "TEST_HANG_BOUND",
    "TEST_HANG_REPORT_GRACE",
];

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found = super::require_sentinels(corpus, &SENTINELS, "test imports");
    for (path, source) in corpus.under(TREE) {
        match violations(source) {
            Ok(list) => found.extend(list.into_iter().map(|(line, name)| {
                format!(
                    "{path}:{line}: {name} is outside the CLI tests' library surface; drive \
                     the binary, or say why with `// library-deliberate: <reason>`"
                )
            })),
            Err(error) => found.push(format!("{path}: unsupported use syntax, {error}")),
        }
    }
    found
}

fn is_marker(comment: &str) -> bool {
    comment
        .strip_prefix("//")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix("library-deliberate:"))
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

/// Code tokens as (text, line): raw identifiers normalized, `::` joined.
fn words(source: &Source) -> Vec<(String, usize)> {
    let code = source.code();
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut i = 0;
    while i < code.len() {
        let tok = &code[i];
        let joined = source.is_punct(tok, ":")
            && code
                .get(i + 1)
                .is_some_and(|n| source.is_punct(n, ":") && n.start == tok.end);
        if joined {
            out.push(("::".to_owned(), tok.line));
            i += 2;
            continue;
        }
        let text = source.name(tok).unwrap_or_else(|| source.text_of(tok));
        out.push((text.to_owned(), tok.line));
        i += 1;
    }
    out
}

fn ident(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A use-tree leaf: segments, `*` for a glob, and whether it is renamed.
type Leaf = (Vec<String>, bool);

struct UseParser<'a> {
    toks: &'a [(String, usize)],
    leaves: Vec<(Leaf, usize)>,
}

impl UseParser<'_> {
    fn at(&self, i: usize) -> Result<&(String, usize), String> {
        self.toks
            .get(i)
            .ok_or_else(|| "unterminated use".to_owned())
    }

    fn tree(&mut self, mut i: usize, prefix: &[String]) -> Result<usize, String> {
        if self.at(i)?.0 == "::" {
            i += 1;
        }
        let (text, line) = self.at(i)?.clone();
        if text == "{" {
            i += 1;
            loop {
                if self.at(i)?.0 == "}" {
                    return Ok(i + 1);
                }
                i = self.tree(i, prefix)?;
                match self.at(i)?.0.as_str() {
                    "," => i += 1,
                    "}" => return Ok(i + 1),
                    other => return Err(format!("use group token {other:?}")),
                }
            }
        }
        if text == "*" {
            let mut path = prefix.to_vec();
            path.push("*".to_owned());
            self.leaves.push(((path, false), line));
            return Ok(i + 1);
        }
        if !ident(&text) {
            return Err(format!("use token {text:?}"));
        }
        i += 1;
        let mut path = prefix.to_vec();
        if text != "self" {
            path.push(text.clone());
        }
        let next = self.at(i)?.0.clone();
        if next == "::" {
            if text == "self" {
                return Err("use path continuing after self".to_owned());
            }
            return self.tree(i + 1, &path);
        }
        if next == "as" {
            let (name, line) = self.at(i + 1)?.clone();
            if !ident(&name) {
                return Err("use rename".to_owned());
            }
            self.leaves.push(((path, true), line));
            return Ok(i + 2);
        }
        self.leaves.push(((path, false), line));
        Ok(i)
    }
}

/// Parse the tree after `use` at `i`: its leaves and the index after `;`.
fn parse_use(toks: &[(String, usize)], i: usize) -> Result<(Vec<(Leaf, usize)>, usize), String> {
    let mut parser = UseParser {
        toks,
        leaves: Vec::new(),
    };
    let end = parser.tree(i, &[])?;
    if parser.at(end)?.0 != ";" {
        return Err(format!("use statement ends at {:?}", parser.at(end)?.0));
    }
    Ok((parser.leaves, end + 1))
}

fn allowed(path: &[String]) -> bool {
    if path.first().map(String::as_str) != Some(ROOT) {
        return true;
    }
    let rest = &path[1..];
    !rest.is_empty()
        && SURFACE.iter().any(|item| {
            let item: Vec<&str> = item.split("::").collect();
            rest.len() >= item.len() && rest.iter().zip(&item).all(|(a, b)| a == b)
        })
}

fn spelled((path, renamed): &Leaf) -> String {
    let mut text = path.join("::");
    if *renamed {
        text.push_str(" as ...");
    }
    text
}

/// (line, path) of every unmarked path outside the surface; an error,
/// naming the line, on use-tree syntax it cannot read.
pub fn violations(source: &Source) -> Result<Vec<(usize, String)>, String> {
    let marked = marked_lines(source);
    let toks = words(source);
    let mut found = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let (text, line) = (&toks[i].0, toks[i].1);
        let prev = if i > 0 { toks[i - 1].0.as_str() } else { "" };
        if text == "use" && prev != "::" {
            let (leaves, next) =
                parse_use(&toks, i + 1).map_err(|e| format!("line {line}: {e}"))?;
            for (leaf, own) in leaves {
                if !allowed(&leaf.0) && !marked.contains(&line) && !marked.contains(&own) {
                    found.push((own, spelled(&leaf)));
                }
            }
            i = next;
            continue;
        }
        if text == "extern"
            && toks.get(i + 1).is_some_and(|t| t.0 == "crate")
            && toks.get(i + 2).is_some_and(|t| t.0 == ROOT)
        {
            if !marked.contains(&line) {
                found.push((line, "extern crate pbfhogg".to_owned()));
            }
            i += 3;
            continue;
        }
        // A path rooted at the crate: not a segment of another path, though
        // a leading `::` (the extern prelude spelling) still roots it.
        let rooted = prev != "::" || i < 2 || !ident(&toks[i - 2].0);
        if text == ROOT && rooted && prev != "." {
            let mut segments = vec![ROOT.to_owned()];
            let mut j = i + 1;
            while j + 1 < toks.len() && toks[j].0 == "::" && ident(&toks[j + 1].0) {
                segments.push(toks[j + 1].0.clone());
                j += 2;
            }
            if segments.len() > 1 && !allowed(&segments) && !marked.contains(&line) {
                found.push((line, segments.join("::")));
            }
            i = j;
            continue;
        }
        i += 1;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::violations;
    use crate::lex::Source;

    fn refused(src: &str) -> Vec<String> {
        violations(&Source::new(src.to_owned()))
            .expect("supported syntax")
            .into_iter()
            .map(|(_, path)| path)
            .collect()
    }

    #[test]
    fn the_surface_and_marked_paths_pass() {
        for src in [
            "use pbfhogg::writer::{Compression, PbfWriter};",
            "use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};",
            "use pbfhogg::writer::PbfWriter as Writer;",
            "use pbfhogg::test_hooks::PublishFault::{self, *};",
            "let w = pbfhogg::writer::PbfWriter::new(f, pbfhogg::writer::Compression::None);",
            "let t = ::pbfhogg::TEST_CHILD_TIMEOUT;",
            "use std::path::Path;\nuse crate::invoker::CliInvoker;",
            "let x = pbfhogg_cli::cli_command();",
            "// pbfhogg::cat::run in a comment\nlet x = 1;",
            "let s = \"pbfhogg::cat::run\";",
            "let x = pbfhogg::cat::run(); // library-deliberate: a reason",
            "use pbfhogg::{\n    writer::PbfWriter,\n    cat::run, // library-deliberate: a reason\n};",
            "use pbfhogg::{ // library-deliberate: a reason\n    cat::run,\n};",
            "let n = self.pbfhogg::<u8>;",
            "    // library-deliberate: a reason\n    let x = pbfhogg::cat::run();",
            "let p = pbfhogg::Info::new();",
        ] {
            assert_eq!(refused(src), Vec::<String>::new(), "{src}");
        }
    }

    #[test]
    fn paths_off_the_surface_are_refused() {
        let cases: [(&str, &[&str]); 21] = [
            (
                "use pbfhogg::tags_count::tags_count;",
                &["pbfhogg::tags_count::tags_count"],
            ),
            (
                "use pbfhogg::{\n    writer::PbfWriter,\n    commands::parse_rfc3339_utc,\n};",
                &["pbfhogg::commands::parse_rfc3339_utc"],
            ),
            (
                "use {\n    std::path::Path,\n    pbfhogg::commands::parse_rfc3339_utc,\n};",
                &["pbfhogg::commands::parse_rfc3339_utc"],
            ),
            (
                "use { pbfhogg::{ writer::PbfWriter, cat::run } };",
                &["pbfhogg::cat::run"],
            ),
            (
                "use pbfhogg /* note */ :: commands::parse;",
                &["pbfhogg::commands::parse"],
            ),
            ("use r#pbfhogg::r#cat::run;", &["pbfhogg::cat::run"]),
            (
                "let p = pbfhogg::Parallelism::from_jobs(1);",
                &["pbfhogg::Parallelism::from_jobs"],
            ),
            (
                "use pbfhogg::writer as writer;",
                &["pbfhogg::writer as ..."],
            ),
            ("use pbfhogg::writer::*;", &["pbfhogg::writer::*"]),
            (
                "use pbfhogg::writer::{self, PbfWriter};",
                &["pbfhogg::writer"],
            ),
            ("use pbfhogg::block_builder;", &["pbfhogg::block_builder"]),
            ("use pbfhogg as p;", &["pbfhogg as ..."]),
            ("use pbfhogg::*;", &["pbfhogg::*"]),
            ("extern crate pbfhogg;", &["extern crate pbfhogg"]),
            (
                "let r = pbfhogg::cat(); let t = \"// library-deliberate: example\";",
                &["pbfhogg::cat"],
            ),
            (
                "let r = pbfhogg::cat(); /* // library-deliberate: example */",
                &["pbfhogg::cat"],
            ),
            (
                "let r = pbfhogg::cat(); // library-deliberate:",
                &["pbfhogg::cat"],
            ),
            (
                "let r = pbfhogg::writer::Other::new();",
                &["pbfhogg::writer::Other::new"],
            ),
            (
                "let r = pbfhogg::BlobReaderish::new();",
                &["pbfhogg::BlobReaderish::new"],
            ),
            (
                "let a = 1; // library-deliberate: a reason\nlet r = pbfhogg::cat();",
                &["pbfhogg::cat"],
            ),
            (
                "// library-deliberate: a reason\n\nlet r = pbfhogg::cat();",
                &["pbfhogg::cat"],
            ),
        ];
        for (src, expected) in cases {
            assert_eq!(refused(src), expected, "{src}");
        }
    }

    #[test]
    fn unknown_use_syntax_fails_closed() {
        for src in [
            "use pbfhogg::{writer::PbfWriter",
            "use pbfhogg::writer::PbfWriter as;",
            "use pbfhogg::self::writer;",
            "use pbfhogg::{a b};",
        ] {
            assert!(violations(&Source::new(src.to_owned())).is_err(), "{src}");
        }
    }
}
