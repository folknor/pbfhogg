//! Library static items are forbidden except for a counted inventory.
//!
//! Every exception is a whole declaration, `static` through its terminating
//! `;`, initializer included, and an exact occurrence count, not a file
//! exemption; no fault-plan owner is exempt. Immutable Rust statics can
//! contain interior mutability, so checking only `static mut` is
//! insufficient. Production exceptions are thread-local decoder and framer
//! scratch and output name sequencing; the rest are unit-test helpers.
//!
//! What is scanned: every file under `crates/lib/src/`, plus every file the
//! library's module graph mounts from `lib.rs` under any configuration, so a
//! `#[path]` module outside the directory (a shared test helper) is read as
//! library source. A module the graph cannot place, or a `#[path]` to a file
//! that is not Rust source in the corpus, fails the rule.
//!
//! The scan reads every `static` keyword token, macro input and
//! `macro_rules!` bodies included, independent of feature and module names.
//! A declaration is compared token by token, literal contents preserved, so
//! a changed initializer is a finding; what a called constructor does is not
//! pinned. Unknown shapes (macro metavariable names, `static mut`, an
//! unbalanced declaration) fail closed. Lifetimes and raw identifiers are not
//! the keyword.
//!
//! The boundary, and why it is where it is. A `macro_rules!` transcriber
//! emits only tokens written in its definition or its invocation, both of
//! which this scan reads, so a crate-local declarative macro cannot produce
//! an unseen `static`. Source loading can: `include!` pulls in tokens no
//! scanned file holds, so the identifier `include`, raw or plain, is refused
//! wherever it appears in code (a metavariable can carry it to a `!` the
//! name never sits beside); a re-export of the macro through a dependency
//! under another name is outside this lexical rule. `include_str!` and `include_bytes!` yield literals, not
//! items, and stay legal. The workspace holds no procedural-macro crate. A
//! dependency's macros expand to that dependency's state: `std`'s
//! `thread_local!` is read here through its input, and the `hotpath`
//! profiling attributes' process-wide state is deliberate and documented on
//! the `hotpath` feature in `crates/lib/Cargo.toml`.

use std::collections::{BTreeMap, BTreeSet};

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

const TREE: &str = "crates/lib/src/";
const ROOT: &str = "crates/lib/src/lib.rs";

/// (file, the whole declaration, count). Whitespace and comments are not
/// compared; every other token is, literal contents included.
pub const INVENTORY: &[(&str, &str, usize)] = &[
    (
        "crates/lib/src/fadvise.rs",
        "static ATTEMPTS: RefCell<Vec<(Advice, Range)>> = const { RefCell::new(Vec::new()) };",
        1,
    ),
    (
        "crates/lib/src/write/framing.rs",
        "static PIPELINE_SCRATCH: RefCell<FrameScratch> = \
         const { RefCell::new(FrameScratch::new()) };",
        1,
    ),
    (
        "crates/lib/src/threads.rs",
        "static HELD: Cell<Option<SlowTeardown>> = const { Cell::new(None) };",
        1,
    ),
    (
        "crates/lib/src/error.rs",
        "static FOREIGN_ROOT: ForeignRoot = ForeignRoot;",
        1,
    ),
    (
        "crates/lib/src/output.rs",
        "static SEQ: AtomicU64 = AtomicU64::new(0);",
        1,
    ),
    (
        "crates/lib/src/read/pipeline.rs",
        "static ST_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };",
        1,
    ),
    (
        "crates/lib/src/read/pipeline.rs",
        "static GR_SCRATCH: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };",
        1,
    ),
    (
        "crates/lib/src/read/pipeline.rs",
        "static TEARDOWN: RefCell<Option<Teardown>> = const { RefCell::new(None) };",
        1,
    ),
    (
        "crates/lib/src/read/header_walker.rs",
        "static DIRECT_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };",
        1,
    ),
    (
        "crates/lib/src/read/decompress.rs",
        "static ZLIB_DECOMPRESS: RefCell<Decompress> = RefCell::new(Decompress::new(true));",
        1,
    ),
    // Mounted into the library's unit tests by `#[path]` from
    // `test_support.rs`: the gate's backend requirement, read once.
    (
        "crates/lib/tests/common/skip.rs",
        "static REQUIREMENT: std::sync::OnceLock<Result<Vec<Backend>, String>> = \
         std::sync::OnceLock::new();",
        1,
    ),
];

/// Files whose absence is itself a finding, so a renamed owner cannot
/// silently leave the scan.
const PROTECTED: [&str; 2] = ["crates/lib/src/test_hooks.rs", "crates/lib/src/host.rs"];

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found: Vec<String> = super::require_files(corpus, TREE, "static inventory")
        .into_iter()
        .collect();
    for path in PROTECTED {
        if !corpus.files.contains_key(path) {
            found.push(format!("source invariants: protected file missing: {path}"));
        }
    }
    let mut scanned: BTreeSet<String> = corpus.under(TREE).map(|(p, _)| p.to_owned()).collect();
    match super::test_modules::mounted(corpus, ROOT) {
        Ok(mounted) => scanned.extend(mounted),
        Err(error) => found.push(format!("static inventory: library module graph: {error}")),
    }
    let sources: Vec<(&str, &Source)> = scanned
        .iter()
        .filter_map(|p| corpus.files.get(p).map(|s| (p.as_str(), s)))
        .collect();
    found.extend(inventory_errors(&sources, INVENTORY));
    found
}

/// The compared form of a declaration: its code token texts.
fn tokens(source: &Source, toks: &[Tok]) -> Vec<String> {
    toks.iter().map(|t| source.text_of(t).to_owned()).collect()
}

/// Refuse static state outside `inventory`, audited declarations that went
/// missing, and source loading the scan cannot follow.
pub fn inventory_errors(
    sources: &[(&str, &Source)],
    inventory: &[(&str, &str, usize)],
) -> Vec<String> {
    let mut expected: BTreeMap<&str, BTreeMap<Vec<String>, usize>> = BTreeMap::new();
    let mut found = Vec::new();
    for (path, declaration, count) in inventory {
        let source = Source::new((*declaration).to_owned());
        let code = source.code();
        match items(&source).as_slice() {
            [(_, Some(_))] => {}
            _ => found.push(format!(
                "{path}: inventory entry is not one static declaration: {declaration}"
            )),
        }
        *expected
            .entry(path)
            .or_default()
            .entry(tokens(&source, &code))
            .or_default() += count;
    }
    for path in expected.keys() {
        if !sources.iter().any(|(p, _)| p == path) {
            found.push(format!("{path}: audited static owner missing"));
        }
    }
    for (path, source) in sources {
        for line in includes(source) {
            found.push(format!(
                "{path}:{line}: `include` loads source the static inventory cannot read"
            ));
        }
        let mut remaining = expected.get(path).cloned().unwrap_or_default();
        for (line, declaration) in items(source) {
            match declaration.and_then(|d| remaining.get_mut(&d)) {
                Some(left) if *left > 0 => *left -= 1,
                _ => found.push(format!(
                    "{path}:{line}: unaudited static state; state belongs to the \
                     invocation (fault state in the FaultPlan its token carries)"
                )),
            }
        }
        for (declaration, left) in remaining {
            if left > 0 {
                found.push(format!(
                    "{path}: missing {left} audited static declaration(s): {}",
                    declaration.concat()
                ));
            }
        }
    }
    found
}

/// Lines holding the identifier `include`, raw or plain, anywhere in code.
/// Neighbouring tokens cannot tell the macro from a variable: a
/// metavariable (`load!(include)` expanding `$m!(..)`) reaches it with no
/// `!` beside the name, so every use is refused and no binding may take the
/// name.
fn includes(source: &Source) -> Vec<usize> {
    source
        .code()
        .iter()
        .filter(|tok| source.name(tok) == Some("include"))
        .map(|tok| tok.line)
        .collect()
}

/// (line, declaration tokens) of every `static` keyword; the declaration is
/// `None` unless `static NAME:` opens it and a `;` closes it at delimiter
/// depth zero, with every delimiter balanced in between.
pub fn items(source: &Source) -> Vec<(usize, Option<Vec<String>>)> {
    let code = source.code();
    let mut out = Vec::new();
    for (i, tok) in code.iter().enumerate() {
        if !source.is_word(tok, "static") {
            continue;
        }
        out.push((tok.line, declaration(source, &code[i..])));
    }
    out
}

fn declaration(source: &Source, toks: &[Tok]) -> Option<Vec<String>> {
    let name = toks.get(1)?;
    if name.kind != Kind::Ident || !source.is_punct(toks.get(2)?, ":") {
        return None;
    }
    let mut open: Vec<&str> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        let text = source.text_of(tok);
        if tok.kind == Kind::Punct {
            match text {
                "(" => open.push(")"),
                "[" => open.push("]"),
                "{" => open.push("}"),
                ")" | "]" | "}" => {
                    if open.pop() != Some(text) {
                        return None;
                    }
                }
                ";" if open.is_empty() => return Some(tokens(source, &toks[..=i])),
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{errors, inventory_errors, items};
    use crate::Corpus;
    use crate::lex::Source;

    fn errors_for(sources: &[(&str, &str)], inventory: &[(&str, &str, usize)]) -> Vec<String> {
        let owned: Vec<(&str, Source)> = sources
            .iter()
            .map(|(p, s)| (*p, Source::new((*s).to_owned())))
            .collect();
        let refs: Vec<(&str, &Source)> = owned.iter().map(|(p, s)| (*p, s)).collect();
        inventory_errors(&refs, inventory)
    }

    #[test]
    fn nested_modules_and_macro_bodies_are_scanned() {
        for src in [
            "#[cfg(pbfhogg_test_hooks)] mod renamed { static X: u8 = 0; }",
            "#[cfg(all(test, pbfhogg_test_hooks))] mod a { mod b { static X: u8 = 0; }}",
            "#[cfg(feature = \"hooks\")] mod gated { static X: u8 = 0; }",
            "macro_rules! state { () => { static X: AtomicBool = AtomicBool::new(false); }}",
            "macro_rules! state { ($name:ident) => { static $name: u8 = 0; }}",
            "thread_local! { static X: Cell<u8> = Cell::new(0); }",
            "thread_local! { static X: Cell<u8> = Cell::new(0) }",
            "fn f() { static mut X: u8 = 0; }",
            "pub(crate) static\n X: u8 = 0;",
            "static X: [u8; 2] = [0; 2]",
        ] {
            assert_eq!(errors_for(&[("owner", src)], &[]).len(), 1, "{src}");
        }
    }

    #[test]
    fn comments_literals_lifetimes_and_raw_identifiers_are_not_statics() {
        let src = "// static X: u8 = 0;
        /* static X: u8 = 0; /* nested */ */
        fn f<'static_fake>(x: &'static str) { let r#static = 1; }
        let s = r###\"static X: u8 = 0;\"###;
        let s = \"static X: u8 = 0;\";
        ";
        assert!(items(&Source::new(src.to_owned())).is_empty());
    }

    #[test]
    fn declarations_are_counted_and_present() {
        let src = "static X: AtomicBool = AtomicBool::new(false);";
        let inventory = [(
            "owner",
            "static X: AtomicBool =\n AtomicBool::new(false);",
            1,
        )];
        assert!(errors_for(&[("owner", src)], &inventory).is_empty());
        let twice = src.repeat(2);
        let other = src.replace("AtomicBool", "Other");
        let muts = src.replace("static X", "static mut X");
        let init = src.replace("false", "true");
        let cases: [&[(&str, &str)]; 7] = [
            &[],
            &[("owner", "")],
            &[("owner", &twice)],
            &[("owner", &other)],
            &[("owner", &muts)],
            &[("owner", &init)],
            &[("owner", src), ("renamed", src)],
        ];
        for sources in cases {
            assert!(!errors_for(sources, &inventory).is_empty(), "{sources:?}");
        }
    }

    #[test]
    fn initializers_with_delimiters_and_literals_compare_exactly() {
        let src = "static X: [u8; 2] = const { [b'a'; 2] };\nstatic S: &str = \"a b\";";
        let inventory = [
            ("owner", "static X: [u8; 2] = const { [b'a'; 2] };", 1),
            ("owner", "static S: &str = \"a b\";", 1),
        ];
        assert!(errors_for(&[("owner", src)], &inventory).is_empty());
        let squashed = src.replace("\"a b\"", "\"ab\"");
        assert!(!errors_for(&[("owner", &squashed)], &inventory).is_empty());
        assert!(!errors_for(&[("owner", src)], &[("owner", "static X: u8", 1)]).is_empty());
    }

    #[test]
    fn source_loading_is_refused() {
        for src in [
            "include!(\"state.inc\");",
            "macro_rules! m { () => { include!(\"x.inc\"); } }",
            "use std::include as load;",
            "use std::{include as load};",
            "std::include!(\"x\");",
            "use std::r#include as load;",
            "macro_rules! load { ($m:ident) => { $m!(\"state.inc\"); }; }\nload!(include);",
            "let include = true;",
        ] {
            assert_eq!(errors_for(&[("owner", src)], &[]).len(), 1, "{src}");
        }
        for src in [
            "// include!(\"in a comment\")\nlet s = \"include\";",
            "const T: &str = include_str!(\"t\");",
            "const B: &[u8] = include_bytes!(\"b\");",
        ] {
            assert!(errors_for(&[("owner", src)], &[]).is_empty(), "{src}");
        }
    }

    #[test]
    fn path_mounted_modules_are_library_source() {
        let mounted = Corpus::from_sources([
            (
                "crates/lib/src/lib.rs",
                "#[path = \"../tests/shared.rs\"]\nmod shared;",
            ),
            ("crates/lib/src/host.rs", ""),
            ("crates/lib/src/test_hooks.rs", ""),
            ("crates/lib/tests/shared.rs", "static X: u8 = 0;"),
        ]);
        let found = errors(&mounted);
        assert!(
            found
                .iter()
                .any(|e| e.starts_with("crates/lib/tests/shared.rs:1:")),
            "{found:?}"
        );
        let outside = Corpus::from_sources([
            (
                "crates/lib/src/lib.rs",
                "#[path = \"../state.inc\"]\nmod state;",
            ),
            ("crates/lib/src/host.rs", ""),
            ("crates/lib/src/test_hooks.rs", ""),
        ]);
        assert!(
            errors(&outside)
                .iter()
                .any(|e| e.contains("library module graph")),
            "{:?}",
            errors(&outside)
        );
    }

    #[test]
    fn lines_survive_multiline_comments() {
        let found = items(&Source::new("/* a\n b */\nstatic X: u8 = 0;".to_owned()));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 3);
        assert_eq!(
            found[0].1.as_ref().map(|d| d.concat()).as_deref(),
            Some("staticX:u8=0;")
        );
    }
}
