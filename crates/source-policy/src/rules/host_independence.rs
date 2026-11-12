//! Test fixtures depend on nothing the host happens to have installed.
//!
//! The host rule (AGENTS.md Conventions) promises a standard Linux box and
//! nothing more, and the tests promise less still: no test starts a shell,
//! an interpreter, a borrowed host utility or any other general tool (a
//! check that needs one is a brokkr script check). Their only children are
//! the pbfhogg binary through `CliInvoker` (which arms the promise guard),
//! the harness executable, the osmium oracle and re-executions of the test
//! binary itself. This rule holds every member's `src/` and `tests/` to that:
//!
//! - **Constructors.** Every `Command::new(...)` spelled bare, `process::`
//!   or `std::process::` qualified is one entry of [`CONSTRUCTORS`]: its
//!   file, the path of items enclosing it (inline `mod`, `impl` self type,
//!   `trait` and `fn` names, read from syn's item tree), its program tokens
//!   as written (whitespace and comments dropped, literals untouched), and
//!   how many times it appears. A file syn cannot parse fails.
//!   A literal program (`"sh"`, `"python3"`, `"/bin/cat"`), a second pbfhogg
//!   constructor that would skip the guard, or a constructor in an
//!   unlisted function fails, and so does an entry no longer found. Any
//!   other qualified `Command::new` fails too, except `clap::Command::new`.
//! - **Cargo binaries.** A string literal whose value names `CARGO_BIN_EXE_`
//!   appears only in the program of an inventoried constructor or as the
//!   whole initializer of an [`EXE_CONSTS`] constant (the one module-level
//!   `const` of that name, exactly `env!` of its variable), so no test can
//!   locate the pbfhogg binary by itself (`env!`, `option_env!` or
//!   `std::env::var`). Literal checks read decoded values.
//! - **libc process creation.** The `fork`, `exec*`, `posix_spawn*` and
//!   `popen` names and their `SYS_` syscall numbers, and `libc::system` and
//!   `libc::clone`, appear only in [`PROCESS_CREATION_OWNERS`]: the guard's
//!   filter and its violation probes. A name is matched after `libc::` and
//!   anywhere inside a `use libc ...;`, so an import is caught at the import.
//! - **Authored scripts.** No string literal holds `#!` then `/`: a fixture
//!   that writes a script depends on the host's interpreter.
//!
//! The literal checks skip this module alone ([`PROBE_FILE`]), whose
//! inventory and probes must spell what they refuse; its code is judged.
//!
//! Copying a host binary into a fixture has no rule of its own: with every
//! constructor closed, a copy has no way to start.
//!
//! A lexical tripwire, not a proof: a `Command` renamed by `use ... as`, a
//! constructor built inside macro input or reached through a function
//! pointer, `CommandExt::exec` on an inventoried command, `libc::syscall`
//! with a numeric or separately imported number, and what an inventoried
//! program variable (the oracle's `path`) holds are outside it. That the
//! guard is applied before the pbfhogg child spawns is `CliInvoker`'s own
//! structure (`try_spawn`), not checked here.

use std::ops::Range;

use syn::Lit;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

/// One permitted child constructor.
pub struct Constructor {
    pub file: &'static str,
    /// Enclosing item names joined by `::`.
    pub owner: &'static str,
    /// The argument's code tokens as written, whitespace and comments
    /// dropped, one space kept only between two word-like tokens.
    pub program: &'static str,
    pub count: usize,
}

const CURRENT_EXE_TEST_BINARY: &str = r#"std::env::current_exe().expect("this test binary")"#;

pub const CONSTRUCTORS: &[Constructor] = &[
    Constructor {
        file: "crates/cli/tests/invoker.rs",
        owner: "CliInvoker::new",
        program: r#"env!("CARGO_BIN_EXE_pbfhogg")"#,
        count: 1,
    },
    Constructor {
        file: "crates/cli/tests/invoker.rs",
        owner: "tests::spawn_gated_child",
        program: CURRENT_EXE_TEST_BINARY,
        count: 1,
    },
    Constructor {
        file: "crates/cli/tests/guard.rs",
        owner: "tests::cases::run_probe",
        program: CURRENT_EXE_TEST_BINARY,
        count: 1,
    },
    Constructor {
        file: "crates/lib/tests/common/oracle.rs",
        owner: "Oracle::new",
        program: "path",
        count: 1,
    },
    Constructor {
        file: "crates/harness/tests/exe.rs",
        owner: "describe_prints_one_valid_document_and_nothing_else",
        program: "EXE",
        count: 1,
    },
    Constructor {
        file: "crates/harness/tests/exe.rs",
        owner: "RunDir::launch",
        program: "EXE",
        count: 1,
    },
    Constructor {
        file: "crates/harness/tests/exe.rs",
        owner: "describe_ignores_a_profiling_request",
        program: "EXE",
        count: 1,
    },
    Constructor {
        file: "crates/harness/tests/exe.rs",
        owner: "describe_scenario",
        program: "EXE",
        count: 1,
    },
    Constructor {
        file: "crates/harness/tests/exe.rs",
        owner: "every_declared_scenario_executes",
        program: "EXE",
        count: 1,
    },
    Constructor {
        file: "crates/harness/tests/nofile.rs",
        owner: "real_descriptor_contract_in_restricted_child",
        program: r#"std::env::current_exe().expect("test executable")"#,
        count: 1,
    },
    Constructor {
        file: "crates/test-scratch/src/hang.rs",
        owner: "tests::an_expired_bound_aborts_the_binary_with_a_named_report",
        program: r#"std::env::current_exe().expect("test binary")"#,
        count: 1,
    },
];

/// The one module-level `const NAME` of `file`, whose whole initializer is
/// exactly `env!(var)`.
pub struct ExeConst {
    pub file: &'static str,
    pub name: &'static str,
    pub var: &'static str,
}

pub const EXE_CONSTS: &[ExeConst] = &[ExeConst {
    file: "crates/harness/tests/exe.rs",
    name: "EXE",
    var: "CARGO_BIN_EXE_pbfhogg-harness",
}];

/// (file, enclosing item path) where libc process creation may be named.
pub const PROCESS_CREATION_OWNERS: &[(&str, &str)] = &[
    ("crates/cli/tests/guard.rs", "filter::program"),
    ("crates/cli/tests/guard.rs", "tests::cases::probe"),
    ("crates/cli/tests/guard.rs", "tests::cases::clone3_ignored"),
    ("crates/cli/tests/guard.rs", "tests::cases::allowed"),
];

/// The one file whose string literals are not judged: this module, whose
/// inventory and probes must spell `CARGO_BIN_EXE_` and script headers. Its
/// code is judged like any other file's.
pub const PROBE_FILE: &str = "crates/source-policy/src/rules/host_independence.rs";

/// Names that start a process wherever they are spelled.
const SPAWN_NAMES: &[&str] = &[
    "fork",
    "vfork",
    "execv",
    "execve",
    "execvp",
    "execvpe",
    "execl",
    "execle",
    "execlp",
    "fexecve",
    "execveat",
    "posix_spawn",
    "posix_spawnp",
    "popen",
    "SYS_fork",
    "SYS_vfork",
    "SYS_clone",
    "SYS_clone3",
    "SYS_execve",
    "SYS_execveat",
];

/// Names too common to refuse bare, refused after `libc::`.
const LIBC_ONLY_NAMES: &[&str] = &["system", "clone", "clone3"];

pub struct Policy<'a> {
    pub constructors: &'a [Constructor],
    pub exe_consts: &'a [ExeConst],
    pub process_creation_owners: &'a [(&'a str, &'a str)],
    pub probe_file: &'a str,
}

pub const POLICY: Policy<'static> = Policy {
    constructors: CONSTRUCTORS,
    exe_consts: EXE_CONSTS,
    process_creation_owners: PROCESS_CREATION_OWNERS,
    probe_file: PROBE_FILE,
};

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found: Vec<String> = super::require_files(corpus, "crates/", "host independence")
        .into_iter()
        .collect();
    found.extend(check(corpus, &POLICY));
    found
}

/// Every finding of `policy` over `corpus`.
pub fn check(corpus: &Corpus, policy: &Policy<'_>) -> Vec<String> {
    let mut found = Vec::new();
    let mut seen = vec![0_usize; policy.constructors.len()];
    for (path, source) in corpus.under("crates/") {
        let items = match items(source, path, policy) {
            Ok(items) => items,
            Err(error) => {
                found.push(error);
                continue;
            }
        };
        let file = File {
            path,
            source,
            code: source.code(),
            items,
        };
        let code = &file.code;
        let mut program_ranges = Vec::new();
        for i in 0..code.len() {
            if let Some(err) = constructor(&file, i, policy, &mut seen, &mut program_ranges) {
                found.push(err);
            }
        }
        if path != policy.probe_file {
            found.extend(literal_findings(&file, &program_ranges));
        }
        // Inside a `use` statement, from the first `libc` it names to its
        // `;`: rooted, grouped and nested imports alike. A later name in
        // the same statement from another crate is judged too, a
        // deliberate over-reach.
        let (mut in_use, mut in_libc_use) = (false, false);
        for (i, tok) in code.iter().enumerate() {
            if source.is_word(tok, "use") {
                in_use = true;
            } else if source.is_punct(tok, ";") {
                in_use = false;
                in_libc_use = false;
            } else if in_use && source.is_word(tok, "libc") {
                in_libc_use = true;
            }
            if let Some(name) = process_creation(source, code, i, in_libc_use) {
                let owner = file.items.owner(tok.start);
                if !policy
                    .process_creation_owners
                    .iter()
                    .any(|(f, o)| *f == path && *o == owner)
                {
                    found.push(format!(
                        "{path}:{}: {name} creates a process; only the promise guard's \
                         filter and probes may name it",
                        tok.line
                    ));
                }
            }
        }
    }
    for (entry, count) in policy.constructors.iter().zip(&seen) {
        if *count != entry.count {
            found.push(format!(
                "{}: inventoried child constructor in {} with program `{}` found {count} \
                 times, expected {}",
                entry.file, entry.owner, entry.program, entry.count
            ));
        }
    }
    found
}

/// Script headers in any literal, and `CARGO_BIN_EXE_` outside an
/// inventoried constructor's program (token-index `program_ranges`) or an
/// inventoried `EXE` initializer, judged on decoded literal values.
fn literal_findings(file: &File<'_>, program_ranges: &[Range<usize>]) -> Vec<String> {
    let mut found = Vec::new();
    let path = file.path;
    for (i, tok) in file.code.iter().enumerate() {
        if tok.kind != Kind::Str {
            continue;
        }
        let Some(value) = decoded(file.source.text_of(tok)) else {
            found.push(format!("{path}:{}: undecodable string literal", tok.line));
            continue;
        };
        let owned = program_ranges.iter().any(|r| r.contains(&i))
            || file
                .items
                .exe_initializers
                .iter()
                .any(|r| r.start <= tok.start && tok.end <= r.end);
        if value.contains("CARGO_BIN_EXE_") && !owned {
            found.push(format!(
                "{path}:{}: CARGO_BIN_EXE_ is named outside an inventoried child \
                 constructor; start pbfhogg through CliInvoker",
                tok.line
            ));
        }
        if has_shebang(&value) {
            found.push(format!(
                "{path}:{}: a string literal holds a #! script header; a fixture \
                 that writes a script depends on the host's interpreter",
                tok.line
            ));
        }
    }
    found
}

/// One file being judged.
struct File<'a> {
    path: &'a str,
    source: &'a Source,
    code: Vec<Tok>,
    items: Items,
}

/// The complete path before `Command` at `i`, segments in order, and
/// whether it is rooted by a leading `::`. `None` when the path continues
/// through something other than plain names (`<T as X>::`, a method).
fn path_before(source: &Source, code: &[Tok], i: usize) -> Option<(Vec<String>, bool)> {
    let colons = |j: usize| {
        j >= 2 && source.is_punct(&code[j - 1], ":") && source.is_punct(&code[j - 2], ":")
    };
    let mut segments = Vec::new();
    let mut j = i;
    while colons(j) {
        match j.checked_sub(3).and_then(|k| source.name(&code[k])) {
            Some(name) => {
                segments.push(name.to_owned());
                j -= 3;
            }
            None => break,
        }
    }
    let rooted = colons(j);
    if rooted {
        j -= 2;
    }
    if j > 0 && (source.is_punct(&code[j - 1], ">") || source.is_punct(&code[j - 1], ".")) {
        return None;
    }
    segments.reverse();
    Some((segments, rooted))
}

fn constructor(
    file: &File<'_>,
    i: usize,
    policy: &Policy<'_>,
    seen: &mut [usize],
    program_ranges: &mut Vec<Range<usize>>,
) -> Option<String> {
    let (source, code, path) = (file.source, &file.code[..], file.path);
    let at = |j: usize, word: &str| code.get(j).is_some_and(|t| source.is_word(t, word));
    let punct = |j: usize, p: &str| code.get(j).is_some_and(|t| source.is_punct(t, p));
    if !(at(i, "Command")
        && punct(i + 1, ":")
        && punct(i + 2, ":")
        && at(i + 3, "new")
        && punct(i + 4, "("))
    {
        return None;
    }
    let line = code[i].line;
    // The whole path before `Command`: nothing, `process`, or
    // `std::process` (optionally rooted); exactly `clap::Command` is clap's
    // builder. Anything else is refused.
    let qualification = path_before(source, code, i);
    let segments: Option<Vec<&str>> = qualification
        .as_ref()
        .map(|(s, _)| s.iter().map(String::as_str).collect());
    let rooted = qualification.as_ref().is_some_and(|(_, r)| *r);
    match (segments.as_deref(), rooted) {
        (Some(["clap"]), false) => return None,
        (Some([] | ["process"]), false) | (Some(["std", "process"]), _) => {}
        _ => {
            return Some(format!(
                "{path}:{line}: Command::new under an unrecognised path; spell it \
                 std::process::Command and inventory it"
            ));
        }
    }
    let close = matching_paren(source, code, i + 4)?;
    let program = spelled(source, &code[i + 5..close]);
    let owner = file.items.owner(code[i].start);
    match policy
        .constructors
        .iter()
        .position(|c| c.file == path && c.owner == owner && c.program == program)
    {
        Some(k) => {
            seen[k] += 1;
            program_ranges.push(i + 5..close);
            None
        }
        None => Some(format!(
            "{path}:{line}: child constructor `Command::new({program})` in `{owner}` is not \
             inventoried; tests start only pbfhogg through CliInvoker, the harness, the osmium \
             oracle or their own binary, never a shell, interpreter or host utility"
        )),
    }
}

/// Code tokens as written with whitespace and comments dropped: adjacent
/// tokens are joined directly, except that two word-like tokens (names,
/// literals, numbers) keep one space, so literal contents are untouched and
/// `a b` never reads as `ab`.
fn spelled(source: &Source, toks: &[Tok]) -> String {
    let wordy = |t: &Tok| !matches!(t.kind, Kind::Punct);
    let mut out = String::new();
    for (k, tok) in toks.iter().enumerate() {
        if k > 0 && wordy(&toks[k - 1]) && wordy(tok) {
            out.push(' ');
        }
        out.push_str(source.text_of(tok));
    }
    out
}

/// The spelled process-creation name at `i`, if any: right after `libc ::`,
/// or anywhere inside a `use libc ...;` (grouped imports included), so the
/// import of a name a later bare call uses is what fires. A bare name
/// elsewhere (proptest's `fork` config field) is not judged.
fn process_creation<'s>(
    source: &'s Source,
    code: &[Tok],
    i: usize,
    in_libc_use: bool,
) -> Option<&'s str> {
    let name = source.name(&code[i])?;
    let after_libc = i >= 3
        && source.is_punct(&code[i - 1], ":")
        && source.is_punct(&code[i - 2], ":")
        && source.is_word(&code[i - 3], "libc");
    let named = SPAWN_NAMES.contains(&name) || LIBC_ONLY_NAMES.contains(&name);
    (named && (after_libc || in_libc_use)).then_some(name)
}

/// `#!`, optional blanks, then `/`, anywhere in a literal's text.
fn has_shebang(text: &str) -> bool {
    text.match_indices("#!").any(|(at, _)| {
        text[at + 2..]
            .trim_start_matches([' ', '\t'])
            .starts_with('/')
    })
}

/// The file's item structure, from syn: each named item's byte range with
/// its path, and the byte ranges of inventoried `EXE` initializers.
struct Items {
    spans: Vec<(Range<usize>, String)>,
    exe_initializers: Vec<Range<usize>>,
}

impl Items {
    /// The path of the innermost named item holding byte `offset`.
    fn owner(&self, offset: usize) -> &str {
        self.spans
            .iter()
            .filter(|(r, _)| r.contains(&offset))
            .max_by_key(|(r, _)| r.start)
            .map_or("", |(_, p)| p.as_str())
    }
}

fn range(source: &Source, node: &impl Spanned) -> Range<usize> {
    let span = node.span();
    let (start, end) = (span.start(), span.end());
    source.offset_of(start.line, start.column)..source.offset_of(end.line, end.column)
}

/// Owners are `mod` (inline), `fn`, `trait` names and an `impl`'s self
/// type (its last path segment), joined by `::`.
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

fn items(source: &Source, path: &str, policy: &Policy<'_>) -> Result<Items, String> {
    let file = syn::parse_file(&source.text)
        .map_err(|e| format!("{path}:{}: does not parse: {e}", e.span().start().line))?;
    let mut walk = Walk {
        source,
        stack: Vec::new(),
        spans: Vec::new(),
    };
    walk.visit_file(&file);
    let mut exe_initializers = Vec::new();
    for entry in policy.exe_consts.iter().filter(|c| c.file == path) {
        let matching: Vec<&syn::ItemConst> = file
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Const(c) if c.ident == entry.name => Some(c),
                _ => None,
            })
            .collect();
        let [item] = matching[..] else {
            return Err(format!(
                "{path}: expected exactly one module-level const {}, found {}",
                entry.name,
                matching.len()
            ));
        };
        let var = match &*item.expr {
            syn::Expr::Macro(m) if m.mac.path.is_ident("env") => {
                syn::parse2::<syn::LitStr>(m.mac.tokens.clone())
                    .ok()
                    .map(|s| s.value())
            }
            _ => None,
        };
        if var.as_deref() != Some(entry.var) {
            return Err(format!(
                "{path}: const {} must be exactly env!({:?})",
                entry.name, entry.var
            ));
        }
        exe_initializers.push(range(source, &*item.expr));
        // No other binding of the name, so an inventoried `Command::new(EXE)`
        // cannot reach a shadow: `let EXE`, `const EXE` or `static EXE`
        // anywhere else, or `EXE:` as a parameter or field.
        let code = source.code();
        let shadows = code.iter().enumerate().filter(|(k, t)| {
            source.is_word(t, entry.name)
                && (k.checked_sub(1).is_some_and(|p| {
                    ["let", "mut", "ref", "const", "static"]
                        .iter()
                        .any(|w| source.is_word(&code[p], w))
                }) || code.get(k + 1).is_some_and(|n| source.is_punct(n, ":"))
                    && !code.get(k + 2).is_some_and(|n| source.is_punct(n, ":")))
        });
        if shadows.count() != 1 {
            return Err(format!(
                "{path}: {} is bound more than once; an inventoried constructor's \
                 program must be the one module-level constant",
                entry.name
            ));
        }
    }
    Ok(Items {
        spans: walk.spans,
        exe_initializers,
    })
}

/// A string literal's value: cooked escapes decoded, raw contents as written.
fn decoded(text: &str) -> Option<String> {
    match syn::parse_str::<Lit>(text).ok()? {
        Lit::Str(s) => Some(s.value()),
        Lit::ByteStr(s) => Some(String::from_utf8_lossy(&s.value()).into_owned()),
        Lit::CStr(s) => Some(s.value().to_string_lossy().into_owned()),
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::{Constructor, ExeConst, Policy, check};
    use crate::Corpus;

    const FILE: &str = "crates/x/tests/t.rs";

    const POLICY: Policy<'static> = Policy {
        constructors: &[
            Constructor {
                file: FILE,
                owner: "Invoker::new",
                program: r#"env!("CARGO_BIN_EXE_pbfhogg")"#,
                count: 1,
            },
            Constructor {
                file: FILE,
                owner: "tests::reexec",
                program: "std::env::current_exe().unwrap()",
                count: 1,
            },
            Constructor {
                file: FILE,
                owner: "literal",
                program: r#""pbfhogg""#,
                count: 1,
            },
        ],
        exe_consts: &[ExeConst {
            file: FILE,
            name: "EXE",
            var: "CARGO_BIN_EXE_pbfhogg-harness",
        }],
        process_creation_owners: &[(FILE, "probe")],
        probe_file: "crates/x/src/exempt.rs",
    };

    /// The inventoried constructors, so the count check is satisfied and
    /// only the extra text is judged.
    const BASE: &str = r#"
        use std::process::Command;
        impl<'a> Invoker<'a> {
            pub fn new() -> Self { let c = Command::new(env!("CARGO_BIN_EXE_pbfhogg")); }
        }
        mod tests {
            fn reexec(x: impl Into<u8>) { std::process::Command::new(std::env::current_exe().unwrap()); }
        }
        fn literal() { Command::new("pbfhogg"); }
        fn probe() { unsafe { libc::fork(); libc::syscall(libc::SYS_clone3); } }
        const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-harness");
    "#;

    fn findings(extra: &str) -> Vec<String> {
        findings_at(FILE, extra)
    }

    /// The base with `from` replaced by `to`, so an inventoried call is
    /// changed rather than one appended beside it.
    fn replaced(from: &str, to: &str) -> Vec<String> {
        assert!(BASE.contains(from), "{from}");
        let text = BASE.replace(from, to);
        check(&Corpus::from_sources([(FILE, text.as_str())]), &POLICY)
    }

    #[test]
    fn owners_come_from_items_not_braces() {
        for (from, to) in [
            ("fn reexec(x: impl Into<u8>)", "fn reexec(_: [u8; 1])"),
            (
                "fn reexec(x: impl Into<u8>)",
                "fn reexec<const N: usize = { 1 }>()",
            ),
            (
                "fn reexec(x: impl Into<u8>)",
                "fn reexec<T>() where [u8; { 2 }]: Sized",
            ),
            ("impl<'a> Invoker<'a>", "impl Factory for Invoker"),
            ("impl<'a> Invoker<'a>", "impl crate::Invoker"),
            ("impl<'a> Invoker<'a>", "impl<F: Fn() -> u8> Invoker<F>"),
        ] {
            assert_eq!(replaced(from, to), Vec::<String>::new(), "{to}");
        }
        // A trait impl names its self type, so a same-named trait method
        // elsewhere is another owner.
        // Unlisted constructor, its binary named outside the owner, and the
        // inventoried one missing.
        let found = replaced("impl<'a> Invoker<'a>", "impl Invoker for Other");
        assert_eq!(found.len(), 3, "{found:?}");
    }

    #[test]
    fn qualification_is_judged_on_the_whole_path() {
        let call = "std::process::Command::new(std::env::current_exe().unwrap())";
        for to in [
            "anything::std::process::Command::new(std::env::current_exe().unwrap())",
            "anything::clap::Command::new(std::env::current_exe().unwrap())",
            "<T as X>::Command::new(std::env::current_exe().unwrap())",
        ] {
            assert!(!replaced(call, to).is_empty(), "{to}");
        }
        assert!(replaced(call, &format!("::{call}")).is_empty());
        assert!(
            replaced(
                call,
                "process::Command::new(std::env::current_exe().unwrap())"
            )
            .is_empty()
        );
        assert!(findings("fn f() { let s = S { cmd: clap::Command::new(\"x\") }; }").is_empty());
    }

    #[test]
    fn literal_programs_compare_by_value_spelling() {
        let found = replaced(r#"Command::new("pbfhogg")"#, r#"Command::new("pbf hogg")"#);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            replaced(
                r#"Command::new("pbfhogg")"#,
                r#"Command::new(/* c */ "pbfhogg")"#
            )
            .is_empty()
        );
    }

    #[test]
    fn the_exe_constant_is_its_whole_initializer() {
        let exe = r#"const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-harness");"#;
        for to in [
            r#"const EXE: &str = { const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-harness"); "/bin/sh" };"#,
            r#"const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg");"#,
            r#"const EXE: &str = option_env!("CARGO_BIN_EXE_pbfhogg-harness");"#,
            r#"const EXE: &str = "sh";"#,
            "",
            r#"const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-harness"); mod m { const EXE: &str = env!("CARGO_BIN_EXE_pbfhogg-harness"); }"#,
        ] {
            assert!(!replaced(exe, to).is_empty(), "{to}");
        }
        // A shadow would let an inventoried `Command::new(EXE)` start
        // something else.
        for extra in [
            "fn f() { let EXE = \"sh\"; }",
            "fn f() { let mut EXE = \"sh\"; }",
            "fn f(EXE: &str) {}",
            "fn f() { static EXE: &str = \"sh\"; }",
        ] {
            assert!(!findings(extra).is_empty(), "{extra}");
        }
        assert!(findings("fn f() { let a = EXE; g(EXE::LEN); }").is_empty());
    }

    #[test]
    fn literal_values_are_decoded() {
        for extra in [
            "fn f() { write(p, b\"#!\\t/bin/sh\\n\"); }",
            "fn f() { write(p, b\"\\x23!/bin/sh\\n\"); }",
            "fn f() { option_env!(\"CARGO_BIN_\\x45XE_pbfhogg\"); }",
            "fn f() { write(p, c\"#!/bin/sh\"); }",
        ] {
            assert_eq!(findings(extra).len(), 1, "{extra}: {:?}", findings(extra));
        }
    }

    #[test]
    fn a_file_that_does_not_parse_fails() {
        assert_eq!(findings_at("crates/x/tests/u.rs", "fn broken( {").len(), 1);
    }

    fn findings_at(path: &str, extra: &str) -> Vec<String> {
        let mut sources = vec![(FILE, BASE.to_owned())];
        if path == FILE {
            sources[0].1.push_str(extra);
        } else {
            sources.push((path, extra.to_owned()));
        }
        let corpus = Corpus::from_sources(sources.iter().map(|(p, s)| (*p, s.as_str())));
        check(&corpus, &POLICY)
    }

    #[test]
    fn the_inventoried_base_passes() {
        assert_eq!(findings(""), Vec::<String>::new());
    }

    #[test]
    fn stand_ins_and_unlisted_constructors_fire() {
        for extra in [
            r#"fn f() { Command::new("sh"); }"#,
            r#"fn f() { Command::new("python3").arg("x.py"); }"#,
            r#"fn f() { Command::new(r"perl"); }"#,
            r#"fn f() { Command::new(("node")); }"#,
            r#"fn f() { std::process::Command::new("/bin/cat"); }"#,
            r#"fn f() { ::std::process::Command::new("true"); }"#,
            r#"fn f() { process::Command::new("sleep"); }"#,
            "fn f() { Command::new(std::env::current_exe().unwrap()); }",
            r#"fn f() { other::Command::new("x"); }"#,
            r#"fn f() { Command::new("pbf hogg"); }"#,
        ] {
            assert_eq!(findings(extra).len(), 1, "{extra}: {:?}", findings(extra));
        }
    }

    #[test]
    fn a_second_guard_skipping_pbfhogg_constructor_fires() {
        // Same owner, same program: the count is exceeded.
        let extra =
            r#"impl<'a> Invoker<'a> { fn new() { Command::new(env!("CARGO_BIN_EXE_pbfhogg")); } }"#;
        let found = findings(extra);
        assert!(
            found.iter().any(|f| f.contains("found 2 times")),
            "{found:?}"
        );
        // Another impl's `new` is another owner.
        let extra = r#"impl Other { fn new() { Command::new(env!("CARGO_BIN_EXE_pbfhogg")); } }"#;
        assert!(!findings(extra).is_empty());
        // The same constructor in another file.
        let found = findings_at(
            "crates/x/tests/u.rs",
            r#"impl<'a> Invoker<'a> { fn new() { Command::new(env!("CARGO_BIN_EXE_pbfhogg")); } }"#,
        );
        assert!(!found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_missing_inventoried_constructor_fires() {
        // An empty file: no EXE constant, and each of the three
        // inventoried constructors found 0 times.
        let found = check(&Corpus::from_sources([(FILE, "")]), &POLICY);
        assert_eq!(found.len(), 4, "{found:?}");
        assert_eq!(
            found.iter().filter(|f| f.contains("found 0 times")).count(),
            3
        );
    }

    #[test]
    fn the_binary_is_named_only_by_its_owners() {
        for extra in [
            r#"fn f() { let p = env!("CARGO_BIN_EXE_pbfhogg"); }"#,
            r#"fn f() { let p = option_env!("CARGO_BIN_EXE_pbfhogg"); }"#,
            r#"fn f() { std::env::var("CARGO_BIN_EXE_pbfhogg"); }"#,
            r#"const OTHER: &str = env!("CARGO_BIN_EXE_pbfhogg");"#,
            r#"fn f() { const OTHER: &str = env!("CARGO_BIN_EXE_pbfhogg"); }"#,
        ] {
            assert_eq!(findings(extra).len(), 1, "{extra}: {:?}", findings(extra));
        }
        // Comments and doc text never fire.
        assert!(
            findings("// env!(\"CARGO_BIN_EXE_pbfhogg\")\n/// CARGO_BIN_EXE_pbfhogg\nfn g() {}")
                .is_empty()
        );
    }

    #[test]
    fn process_creation_fires_outside_its_owners() {
        for extra in [
            "fn f() { unsafe { libc::fork(); } }",
            "use libc::fork;",
            "use libc::{execve, getpid};",
            "use ::libc::{fork};",
            "use ::libc::fork;",
            "use {libc::{fork}};",
            "use {std::io, ::libc::{getpid, vfork}};",
            "fn f() { unsafe { libc::syscall(libc::SYS_execve); } }",
            "fn f() { unsafe { libc::posix_spawnp(); } }",
            "fn f() { unsafe { libc::system(c); } }",
            "fn f() { unsafe { libc::clone(c); } }",
        ] {
            assert_eq!(findings(extra).len(), 1, "{extra}: {:?}", findings(extra));
        }
        for extra in [
            "fn f() { x.clone(); system(); }",
            "fn f() { forked(); libc::getpid(); }",
            "fn f() { let c = Config { fork: false }; assert!(!c.fork); }",
            "use libc::getpid;\nfn f() { let fork = 1; }",
            "// libc::fork()\nfn g() { let s = \"fork\"; }",
        ] {
            assert!(findings(extra).is_empty(), "{extra}: {:?}", findings(extra));
        }
    }

    #[test]
    fn script_headers_fire_in_literals_only() {
        for extra in [
            "fn f() { write(p, \"#!/bin/sh\\nexit 0\"); }",
            "fn f() { write(p, b\"#! /usr/bin/env python3\"); }",
            "fn f() { write(p, r#\"x\n#!/bin/bash\"#); }",
        ] {
            assert_eq!(findings(extra).len(), 1, "{extra}: {:?}", findings(extra));
        }
        assert!(findings_at("crates/x/tests/u.rs", "#![allow(unused)]\nfn g() {}").is_empty());
        for extra in [
            "// #!/bin/sh\nfn g() {}",
            "fn f() { let s = \"#![doc]\"; }",
            "fn f() { clap::Command::new(\"pbfhogg\"); }",
        ] {
            assert!(findings(extra).is_empty(), "{extra}: {:?}", findings(extra));
        }
    }

    #[test]
    fn the_literal_exemption_is_one_file() {
        let literals = "fn f() { let s = \"#!/bin/sh\"; let p = env!(\"CARGO_BIN_EXE_pbfhogg\"); }";
        assert!(findings_at("crates/x/src/exempt.rs", literals).is_empty());
        assert_eq!(findings_at("crates/x/src/exempt2.rs", literals).len(), 2);
        assert_eq!(findings_at("crates/y/src/exempt.rs", literals).len(), 2);
        // The exemption covers literals only: code is judged.
        for code in ["fn f() { Command::new(x); }", "use libc::fork;"] {
            assert_eq!(
                findings_at("crates/x/src/exempt.rs", code).len(),
                1,
                "{code}"
            );
        }
    }
}
