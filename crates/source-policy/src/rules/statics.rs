//! Library static items are forbidden except for a counted inventory.
//!
//! Every exception is a declaration header and an exact occurrence count,
//! not a file exemption; no fault-plan owner is exempt. Immutable Rust
//! statics can contain interior mutability, so checking only `static mut`
//! is insufficient. Production exceptions are thread-local decoder and
//! framer scratch and output name sequencing; the rest are unit-test helpers.
//!
//! The scan reads every `static` keyword token in the file, macro input and
//! `macro_rules!` bodies included, independent of feature and module names.
//! Unknown headers (macro metavariables, `static mut`) fail closed. Lifetimes
//! and raw identifiers are not the keyword. A macro synthesizing the keyword,
//! an external procedural macro, a changed initializer and global state a
//! dependency supplies are outside this lexical policy.

use std::collections::BTreeMap;

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

const TREE: &str = "crates/lib/src/";

/// (file, whitespace-free header through `=`, count).
pub const INVENTORY: &[(&str, &str, usize)] = &[
    (
        "crates/lib/src/fadvise.rs",
        "static ATTEMPTS: RefCell<Vec<(Advice, Range)>> =",
        1,
    ),
    (
        "crates/lib/src/write/framing.rs",
        "static PIPELINE_SCRATCH: RefCell<FrameScratch> =",
        1,
    ),
    (
        "crates/lib/src/threads.rs",
        "static HELD: Cell<Option<SlowTeardown>> =",
        1,
    ),
    (
        "crates/lib/src/error.rs",
        "static FOREIGN_ROOT: ForeignRoot =",
        1,
    ),
    ("crates/lib/src/output.rs", "static SEQ: AtomicU64 =", 1),
    (
        "crates/lib/src/read/pipeline.rs",
        "static ST_SCRATCH: RefCell<Vec<(u32, u32)>> =",
        1,
    ),
    (
        "crates/lib/src/read/pipeline.rs",
        "static GR_SCRATCH: RefCell<Vec<(u32, u32)>> =",
        1,
    ),
    (
        "crates/lib/src/read/pipeline.rs",
        "static TEARDOWN: RefCell<Option<Teardown>> =",
        1,
    ),
    (
        "crates/lib/src/read/header_walker.rs",
        "static DIRECT_OPENS: std::cell::Cell<usize> =",
        1,
    ),
    (
        "crates/lib/src/read/decompress.rs",
        "static ZLIB_DECOMPRESS: RefCell<Decompress> =",
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
    let sources: Vec<(&str, &Source)> = corpus.under(TREE).collect();
    found.extend(inventory_errors(&sources, INVENTORY));
    found
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect()
}

/// Refuse static state outside `inventory`, and audited declarations that
/// went missing.
pub fn inventory_errors(
    sources: &[(&str, &Source)],
    inventory: &[(&str, &str, usize)],
) -> Vec<String> {
    let mut expected: BTreeMap<&str, BTreeMap<String, usize>> = BTreeMap::new();
    for (path, header, count) in inventory {
        *expected
            .entry(path)
            .or_default()
            .entry(squash(header))
            .or_default() += count;
    }
    let mut found = Vec::new();
    for path in expected.keys() {
        if !sources.iter().any(|(p, _)| p == path) {
            found.push(format!("{path}: audited static owner missing"));
        }
    }
    for (path, source) in sources {
        let mut remaining = expected.get(path).cloned().unwrap_or_default();
        for (line, header) in items(source) {
            match header.and_then(|h| remaining.get_mut(&h)) {
                Some(left) if *left > 0 => *left -= 1,
                _ => found.push(format!(
                    "{path}:{line}: unaudited static state; fault state belongs in \
                     FaultPlan carried by the invocation token"
                )),
            }
        }
        for (header, left) in remaining {
            if left > 0 {
                found.push(format!(
                    "{path}: missing {left} audited static declaration(s): {header}"
                ));
            }
        }
    }
    found
}

/// (line, whitespace-free header) of every `static` keyword; the header is
/// `None` unless `static NAME: TYPE =` follows with no `;`, `{` or `}`.
pub fn items(source: &Source) -> Vec<(usize, Option<String>)> {
    let code = source.code();
    let mut out = Vec::new();
    for (i, tok) in code.iter().enumerate() {
        if !source.is_word(tok, "static") {
            continue;
        }
        out.push((tok.line, header(source, &code[i..])));
    }
    out
}

fn header(source: &Source, toks: &[Tok]) -> Option<String> {
    let name = toks.get(1)?;
    if name.kind != Kind::Ident || !source.is_punct(toks.get(2)?, ":") {
        return None;
    }
    let mut text = String::new();
    for tok in toks {
        if source.is_punct(tok, ";") || source.is_punct(tok, "{") || source.is_punct(tok, "}") {
            return None;
        }
        text.push_str(source.text_of(tok));
        if source.is_punct(tok, "=") {
            return Some(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{inventory_errors, items};
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
            "fn f() { static mut X: u8 = 0; }",
            "pub(crate) static\n X: u8 = 0;",
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
    fn headers_are_counted_and_present() {
        let src = "static X: AtomicBool = AtomicBool::new(false);";
        let inventory = [("owner", "static X: AtomicBool =", 1)];
        assert!(errors_for(&[("owner", src)], &inventory).is_empty());
        let twice = src.repeat(2);
        let other = src.replace("AtomicBool", "Other");
        let muts = src.replace("static X", "static mut X");
        let cases: [&[(&str, &str)]; 6] = [
            &[],
            &[("owner", "")],
            &[("owner", &twice)],
            &[("owner", &other)],
            &[("owner", &muts)],
            &[("owner", src), ("renamed", src)],
        ];
        for sources in cases {
            assert!(!errors_for(sources, &inventory).is_empty(), "{sources:?}");
        }
    }

    #[test]
    fn lines_survive_multiline_comments() {
        assert_eq!(
            items(&Source::new("/* a\n b */\nstatic X: u8 = 0;".to_owned())),
            [(3, Some("staticX:u8=".to_owned()))]
        );
    }
}
