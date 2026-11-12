//! The library's runtime text names no CLI subcommand and no long flag.
//!
//! The library is one front end's engine among possible others, and an
//! error or warning that tells the reader to "run `pbfhogg cat`" or "pass
//! `--force`" makes the library's text a dependency of the CLI's argument
//! surface - wrong for any other caller, and stale the moment a flag is
//! renamed. The library raises a typed error instead and the CLI adds the
//! advice (`exit_code::output_hint`, the diagnostic hints in `host`).
//!
//! Every string literal token under `crates/lib/src/`, outside comments,
//! attributes and test-only code, is decoded and checked for `pbfhogg
//! <word>` independently of the current command names, and for any
//! long-flag shape `--word`.
//!
//! Test-only means gated by an attribute whose cfg predicate *requires*
//! `test` (`#[test]`, `#[cfg(test)]`, `#[cfg(all(test, ..))]`; never
//! `cfg(any(test, ..))`, `cfg(not(test))` or `cfg_attr`) on an item, impl
//! or trait item, field, variant, match arm, `let` or statement macro, or a
//! file or inline module with its own inner test gate. Module declarations
//! never exempt other files: a file can also be included by a production
//! module. The parser only ever grants exemptions, so syntax it keeps
//! opaque (macro input, `macro_rules!` bodies, verbatim nodes) is scanned
//! whole: attribute-shaped tokens there are macro input, not gates.

use std::ops::Range;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Lit, Meta};

use crate::Corpus;
use crate::lex::{Kind, Source};

const TREE: &str = "crates/lib/src/";

/// Long-flag shapes in library literals that are not the CLI's flags. Each
/// entry is (path suffix under `crates/lib/src/`, phrase containing the
/// flag, why); an occurrence is allowed only where the phrase covers it.
const ALLOWED: &[(&str, &str, &str)] = &[(
    "commands/time_filter/mod.rs",
    "osmium --drop-metadata",
    "names a flag of osmium, another tool, as the provenance of stripped metadata",
)];

/// Diagnostic prose naming the program, rather than an invocation. Keep
/// the whole grammatical phrase here, never a bare word that could become a
/// command.
const PROGRAM_PROSE: &[&str] = &[
    "pbfhogg uses for its own staging",
    "pbfhogg never deletes",
    "pbfhogg will not remove",
    "pbfhogg kept (",
    "pbfhogg run that did not finish",
    "pbfhogg run did not finish",
    "pbfhogg does not remove",
    "pbfhogg write paths do",
];

/// One string literal in non-test code: (line, decoded text).
pub type Literal = (usize, String);

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let files: Vec<(&str, &Source)> = corpus.under(TREE).collect();
    if files.len() <= 50 {
        return vec![format!(
            "library text: found only {} files under {TREE}; the scan is looking in the wrong place",
            files.len()
        )];
    }
    let mut problems = Vec::new();
    let mut used = vec![false; ALLOWED.len()];
    for (path, source) in files {
        let literals = match literals(source) {
            Ok(literals) => literals,
            Err(error) => {
                problems.push(format!("{path}: {error}"));
                continue;
            }
        };
        for (line, text) in literals {
            let at = format!("{path}:{line}");
            if let Some(name) = names_subcommand(&text) {
                problems.push(format!("{at}: names `pbfhogg {name}`"));
            }
            for (range, flag) in long_flags(&text) {
                match allowed_at(path, &text, &range) {
                    Some(i) => used[i] = true,
                    None => problems.push(format!("{at}: names `{flag}`")),
                }
            }
        }
    }
    for ((file, phrase, _), used) in ALLOWED.iter().zip(used) {
        if !used {
            problems.push(format!(
                "allow-list entry ({file}, {phrase:?}) matches nothing; remove it"
            ));
        }
    }
    problems
        .into_iter()
        .map(|p| {
            format!("library text names CLI surface; raise a typed error and let the CLI add the advice: {p}")
        })
        .collect()
}

/// Whether a cfg predicate holds only in test builds.
fn requires_test(meta: &Meta) -> bool {
    if meta.path().is_ident("test") {
        return matches!(meta, Meta::Path(_));
    }
    let Meta::List(list) = meta else {
        return false;
    };
    let Ok(args) =
        list.parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
    else {
        return false;
    };
    if list.path.is_ident("all") {
        args.iter().any(requires_test)
    } else if list.path.is_ident("any") {
        !args.is_empty() && args.iter().all(requires_test)
    } else {
        false
    }
}

/// Whether an attribute gates its node to test builds.
fn is_test_attribute(attr: &Attribute) -> bool {
    match &attr.meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("cfg") => list
            .parse_args::<Meta>()
            .is_ok_and(|meta| requires_test(&meta)),
        _ => false,
    }
}

fn outer_test(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| matches!(a.style, syn::AttrStyle::Outer) && is_test_attribute(a))
}

fn inner_test(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| matches!(a.style, syn::AttrStyle::Inner(_)) && is_test_attribute(a))
}

/// Byte ranges the parser proves are not production text: attributes and
/// test-gated nodes.
struct Exempt<'a> {
    source: &'a Source,
    ranges: Vec<Range<usize>>,
}

impl Exempt<'_> {
    fn add(&mut self, node: &impl Spanned) {
        let span = node.span();
        let (start, end) = (span.start(), span.end());
        self.ranges.push(
            self.source.offset_of(start.line, start.column)
                ..self.source.offset_of(end.line, end.column),
        );
    }
}

fn item_attrs(item: &syn::Item) -> &[Attribute] {
    use syn::Item;
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &syn::ImplItem) -> &[Attribute] {
    use syn::ImplItem;
    match item {
        ImplItem::Const(i) => &i.attrs,
        ImplItem::Fn(i) => &i.attrs,
        ImplItem::Type(i) => &i.attrs,
        ImplItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &syn::TraitItem) -> &[Attribute] {
    use syn::TraitItem;
    match item {
        TraitItem::Const(i) => &i.attrs,
        TraitItem::Fn(i) => &i.attrs,
        TraitItem::Type(i) => &i.attrs,
        TraitItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

impl<'ast> Visit<'ast> for Exempt<'_> {
    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        self.add(attr);
    }

    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs = item_attrs(item);
        let inline_test =
            matches!(item, syn::Item::Mod(m) if m.content.is_some()) && inner_test(attrs);
        if outer_test(attrs) || inline_test {
            self.add(item);
        } else {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if outer_test(impl_item_attrs(item)) {
            self.add(item);
        } else {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        if outer_test(trait_item_attrs(item)) {
            self.add(item);
        } else {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_field(&mut self, field: &'ast syn::Field) {
        if outer_test(&field.attrs) {
            self.add(field);
        } else {
            visit::visit_field(self, field);
        }
    }

    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        if outer_test(&variant.attrs) {
            self.add(variant);
        } else {
            visit::visit_variant(self, variant);
        }
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if outer_test(&arm.attrs) {
            self.add(arm);
        } else {
            visit::visit_arm(self, arm);
        }
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if outer_test(&local.attrs) {
            self.add(local);
        } else {
            visit::visit_local(self, local);
        }
    }

    fn visit_stmt_macro(&mut self, stmt: &'ast syn::StmtMacro) {
        if outer_test(&stmt.attrs) {
            self.add(stmt);
        } else {
            visit::visit_stmt_macro(self, stmt);
        }
    }
}

/// Every string literal in `source`'s non-test code, decoded; `Err` when
/// the file does not parse, since then nothing can be exempted soundly.
pub fn literals(source: &Source) -> Result<Vec<Literal>, String> {
    let file = syn::parse_file(&source.text)
        .map_err(|e| format!("line {}: does not parse: {e}", e.span().start().line))?;
    if inner_test(&file.attrs) {
        return Ok(Vec::new());
    }
    let mut exempt = Exempt {
        source,
        ranges: Vec::new(),
    };
    exempt.visit_file(&file);
    let mut out = Vec::new();
    for tok in source.toks.iter().filter(|t| t.kind == Kind::Str) {
        if exempt
            .ranges
            .iter()
            .any(|r| r.start <= tok.start && tok.end <= r.end)
        {
            continue;
        }
        let text = source.text_of(tok);
        let decoded = match syn::parse_str::<Lit>(text) {
            Ok(Lit::Str(s)) => s.value(),
            Ok(Lit::ByteStr(s)) => String::from_utf8_lossy(&s.value()).into_owned(),
            Ok(Lit::CStr(s)) => s.value().to_string_lossy().into_owned(),
            _ => return Err(format!("line {}: undecodable literal {text}", tok.line)),
        };
        out.push((tok.line, decoded));
    }
    Ok(out)
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Invocation-shaped text stays forbidden after a command is renamed or
/// removed: membership in today's command tree cannot establish that.
pub fn names_subcommand(text: &str) -> Option<&str> {
    text.match_indices("pbfhogg").find_map(|(i, _)| {
        if text[..i].chars().next_back().is_some_and(is_ident) {
            return None;
        }
        let phrase = &text[i..];
        if PROGRAM_PROSE.iter().any(|prose| phrase.starts_with(prose)) {
            return None;
        }
        let rest = &phrase["pbfhogg".len()..];
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() || !trimmed.starts_with(|c: char| c.is_ascii_lowercase()) {
            return None;
        }
        let end = trimmed
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .unwrap_or(trimmed.len());
        Some(&trimmed[..end])
    })
}

/// Every long-flag shape (`--word`) in `text`, with its byte range.
pub fn long_flags(text: &str) -> Vec<(Range<usize>, String)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    for (k, &(at, c)) in chars.iter().enumerate() {
        let starts = c == '-'
            && chars.get(k + 1).is_some_and(|(_, c)| *c == '-')
            && chars
                .get(k + 2)
                .is_some_and(|(_, c)| c.is_ascii_lowercase());
        let free =
            k == 0 || !chars[k - 1].1.is_alphanumeric() && !matches!(chars[k - 1].1, '-' | '_');
        if starts && free {
            let flag: String = text[at..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            out.push((at..at + flag.len(), flag));
        }
    }
    out
}

/// Whether the flag occurrence at `range` lies inside an allowed phrase.
fn allowed_at(path: &str, text: &str, range: &Range<usize>) -> Option<usize> {
    ALLOWED.iter().position(|(suffix, phrase, _)| {
        path.ends_with(suffix)
            && text
                .match_indices(phrase)
                .any(|(start, _)| start <= range.start && range.end <= start + phrase.len())
    })
}

#[cfg(test)]
mod tests {
    use super::{allowed_at, literals, long_flags, names_subcommand};
    use crate::lex::Source;

    fn texts(src: &str) -> Vec<String> {
        literals(&Source::new(src.to_owned()))
            .expect("parses")
            .into_iter()
            .map(|(_, t)| t)
            .collect()
    }

    #[test]
    fn the_scanner_sees_what_it_must_and_skips_what_it_must() {
        let src = r##"
        // "pbfhogg cat" in a comment
        /* "--force" in a /* nested */ block */
        fn f() -> &'static str { let _c = '"'; "run pbfhogg cat \
            now" }
        const RAW: &str = r#"pass --force"#;
        const ESC: &str = "\x20 --io-uring";
        #[cfg(test)]
        mod tests { const X: &str = "--hidden"; }
        #[cfg(test)]
        mod gated;
        #[path = "elsewhere.rs"]
        #[cfg(test)]
        mod moved;
        #[cfg(all(test, feature = "x"))]
        fn g() -> Result<(), ()> { let _ = "--hidden-too"; Ok(()) }
        #[cfg(not(test))]
        const LIVE: &str = "--live";
        #[cfg(any(test, feature = "y"))]
        const MAYBE: &str = "--maybe";
        #[cfg_attr(test, allow(dead_code))]
        const ATTR: &str = "--attr";
        struct S { #[cfg(test)] a: u8, b: &'static str }
        message!(#[cfg(test)] "--macro-input");
        macro_rules! m { (#[cfg(test)] $v:literal) => { "--macro-body" }; }
        mod inline { #![cfg(test)] const Y: &str = "--inner"; }
        const UNI: &str = "\u{2d}\u{2d}force";
        #[error("--in-attribute")]
        struct E;
        const AFTER: &str = "after";
    "##;
        assert_eq!(
            texts(src),
            [
                "run pbfhogg cat now",
                "pass --force",
                "  --io-uring",
                "--live",
                "--maybe",
                "--attr",
                "--macro-input",
                "--macro-body",
                "--force",
                "after"
            ],
        );
        let found = literals(&Source::new(src.to_owned())).expect("parses");
        assert_eq!(found[0].0, 4);
        assert!(texts("#![cfg(test)]\nconst X: &str = \"--x\";").is_empty());
        assert!(literals(&Source::new("fn (".to_owned())).is_err());
    }

    #[test]
    fn flags_and_invocations_are_recognized() {
        let flags: Vec<String> = long_flags("a --io-uring b --x")
            .into_iter()
            .map(|(_, f)| f)
            .collect();
        assert_eq!(flags, ["--io-uring", "--x"]);
        assert!(long_flags("a---b x--y <!-- c").is_empty());
        assert_eq!(names_subcommand("e.g. pbfhogg cat --clean"), Some("cat"));
        assert_eq!(names_subcommand("pbfhogg\n   cat"), Some("cat"));
        assert_eq!(names_subcommand("pbfhogg never deletes"), None);
        assert_eq!(names_subcommand("pbfhogg catalogue"), Some("catalogue"));
        // The allow-list covers its own occurrence, not the whole literal.
        let path = "crates/lib/src/commands/time_filter/mod.rs";
        let text = "stripped (osmium --drop-metadata); pass --force";
        let flagged: Vec<String> = long_flags(text)
            .into_iter()
            .filter(|(range, _)| allowed_at(path, text, range).is_none())
            .map(|(_, f)| f)
            .collect();
        assert_eq!(flagged, ["--force"]);
    }

    #[test]
    fn renamed_and_removed_commands_still_count_as_frontend_advice() {
        for name in ["retired-command", "renamed-command", "cat"] {
            assert_eq!(
                names_subcommand(&format!("run pbfhogg {name} input.pbf")),
                Some(name)
            );
        }
        assert_eq!(names_subcommand("other_pbfhogg cat"), None);
        assert_eq!(names_subcommand("pbfhogg 0.5.0"), None);
        assert_eq!(
            names_subcommand("pbfhogg never deletes; run pbfhogg retired"),
            Some("retired")
        );
    }

    #[test]
    fn module_declarations_never_exempt_another_file() {
        // Each physical file is read on its own: a test-gated declaration of
        // a shared file says nothing about that file's own text.
        let entry = "#[cfg(test)]\n#[path = \"shared.rs\"]\nmod test_view;";
        assert!(texts(entry).is_empty());
        assert_eq!(
            texts("const LIVE: &str = \"--production\";"),
            ["--production"]
        );
    }
}
