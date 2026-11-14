//! Nothing in the tree mutates the process environment, and nothing reads
//! it except at a closed inventory of reads, each naming the variable it
//! reads and why.
//!
//! Tests above all: a test's inputs are what it builds, never what the
//! runner happens to export, and a child's environment is exactly what its
//! test sets (every child constructor clears it; see `host_independence`).
//! The library reads nothing (ADR-0016). The production CLI and harness read
//! their protocol inputs, which brokkr and the CLI tests set explicitly.
//! Holding those here too keeps one list of every variable the tree reads,
//! and stops a read moving from a test into a helper unseen. Every member's
//! `src/` and `tests/` is judged:
//!
//! - **Refused everywhere**: `std::env::{set_var, remove_var,
//!   set_current_dir, vars, vars_os, temp_dir, home_dir}` (mutation, bulk
//!   enumeration, and the accessors that read variables indirectly) and libc's
//!   `getenv`, `secure_getenv`, `setenv`, `unsetenv`, `putenv`, `clearenv`
//!   and `environ`, after `libc::` or inside a `use libc ...;`.
//! - **Reads**: `env::var`, `env::var_os` and `env::current_dir` are direct
//!   calls whose one argument is a string literal (a constant could be
//!   shadowed by an import, a glob or a local binding), at an entry of
//!   [`READS`]: file, owner (syn's item path), operation, key and count. A
//!   reference that is not a call (`let read = std::env::var_os;`) fails.
//! - **Imports**: the environment is spelled by path only. A `use` naming
//!   `env`, aliasing or globbing `std` or `libc` themselves (`use std as
//!   x`, `use std::*`, `use std::{self as x}`, `extern crate std as x`)
//!   fails, so no alias reaches the names above.
//!
//! Compile-time `env!` and `option_env!` are build inputs, not the process
//! environment, and are not judged here (`host_independence` governs
//! `CARGO_BIN_EXE_`). A lexical tripwire, not a proof: macro input, a
//! hand-written `extern "C"` declaration, `/proc/self/environ` and what
//! dependencies read on their own are outside it.

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

use super::Owners;
use super::host_independence::decoded;

/// Why a read is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A declared run requirement the runner supplies, strictly parsed, that
    /// can only make a run stricter (a refused backend fails instead of
    /// skipping). `PBFHOGG_TEST_REQUIRE`, set by the gate profile, is the
    /// only one: a runner input that changed fixtures, expected results or
    /// execution parameters would not qualify.
    RunnerRequirement,
    /// Selecting an optional host tool (the osmium oracle): its absence
    /// skips; the parent resolves it to an absolute path and the child
    /// inherits nothing.
    HostTool,
    /// A re-executed test binary reading the gate its parent test set in the
    /// child's explicitly built environment.
    ParentSet,
    /// A production protocol input of the CLI or harness, set by brokkr or
    /// by a CLI test through a named `CliInvoker` setter.
    Protocol,
    /// An operator setting the dataset tool forwards to its transfer child:
    /// `PATH` to find curl, and the proxy and CA settings curl honours, read
    /// at the tool's executable boundary and passed on by name only.
    Operator,
}

/// One admitted read.
pub struct Read {
    pub file: &'static str,
    pub owner: &'static str,
    /// `var`, `var_os` or `current_dir`.
    pub op: &'static str,
    /// The variable, or empty for `current_dir`.
    pub key: &'static str,
    pub count: usize,
    pub reason: Reason,
}

const fn read(
    file: &'static str,
    owner: &'static str,
    op: &'static str,
    key: &'static str,
    reason: Reason,
) -> Read {
    Read {
        file,
        owner,
        op,
        key,
        count: 1,
        reason,
    }
}

const SIGNALS: &str = "crates/cli/src/signals.rs";

const DATASETS: &str = "crates/datasets/src/main.rs";

const VERIFY: &str = "crates/verify/src/main.rs";

pub const READS: &[Read] = &[
    read(VERIFY, "execute", "var_os", "PATH", Reason::Operator),
    read(VERIFY, "curl_env", "var_os", "http_proxy", Reason::Operator),
    read(
        VERIFY,
        "curl_env",
        "var_os",
        "https_proxy",
        Reason::Operator,
    ),
    read(
        VERIFY,
        "curl_env",
        "var_os",
        "HTTPS_PROXY",
        Reason::Operator,
    ),
    read(VERIFY, "curl_env", "var_os", "all_proxy", Reason::Operator),
    read(VERIFY, "curl_env", "var_os", "ALL_PROXY", Reason::Operator),
    read(VERIFY, "curl_env", "var_os", "no_proxy", Reason::Operator),
    read(VERIFY, "curl_env", "var_os", "NO_PROXY", Reason::Operator),
    read(
        VERIFY,
        "curl_env",
        "var_os",
        "SSL_CERT_FILE",
        Reason::Operator,
    ),
    read(
        VERIFY,
        "curl_env",
        "var_os",
        "SSL_CERT_DIR",
        Reason::Operator,
    ),
    read(
        VERIFY,
        "curl_env",
        "var_os",
        "CURL_CA_BUNDLE",
        Reason::Operator,
    ),
    read(
        VERIFY,
        "load_config",
        "var_os",
        "BROKKR_USER_CONFIG",
        Reason::Protocol,
    ),
    read(
        VERIFY,
        "load_config",
        "var_os",
        "XDG_CONFIG_HOME",
        Reason::Protocol,
    ),
    read(VERIFY, "load_config", "var_os", "HOME", Reason::Protocol),
    read(
        VERIFY,
        "boundary",
        "var_os",
        "BROKKR_WITH_EXE",
        Reason::Protocol,
    ),
    read(VERIFY, "boundary", "var_os", "PATH", Reason::Operator),
    read(
        DATASETS,
        "boundary",
        "var_os",
        "BROKKR_USER_CONFIG",
        Reason::Protocol,
    ),
    read(
        DATASETS,
        "boundary",
        "var_os",
        "XDG_CONFIG_HOME",
        Reason::Protocol,
    ),
    read(DATASETS, "boundary", "var_os", "HOME", Reason::Protocol),
    read(
        DATASETS,
        "boundary",
        "var_os",
        "BROKKR_WITH_EXE",
        Reason::Protocol,
    ),
    read(DATASETS, "boundary", "var_os", "PATH", Reason::Operator),
    read(
        DATASETS,
        "curl_env",
        "var_os",
        "http_proxy",
        Reason::Operator,
    ),
    read(
        DATASETS,
        "curl_env",
        "var_os",
        "https_proxy",
        Reason::Operator,
    ),
    read(
        DATASETS,
        "curl_env",
        "var_os",
        "HTTPS_PROXY",
        Reason::Operator,
    ),
    read(
        DATASETS,
        "curl_env",
        "var_os",
        "all_proxy",
        Reason::Operator,
    ),
    read(
        DATASETS,
        "curl_env",
        "var_os",
        "ALL_PROXY",
        Reason::Operator,
    ),
    read(DATASETS, "curl_env", "var_os", "no_proxy", Reason::Operator),
    read(DATASETS, "curl_env", "var_os", "NO_PROXY", Reason::Operator),
    read(
        DATASETS,
        "curl_env",
        "var_os",
        "SSL_CERT_FILE",
        Reason::Operator,
    ),
    read(
        DATASETS,
        "curl_env",
        "var_os",
        "SSL_CERT_DIR",
        Reason::Operator,
    ),
    read(
        DATASETS,
        "curl_env",
        "var_os",
        "CURL_CA_BUNDLE",
        Reason::Operator,
    ),
    read(
        "crates/library/tests/worker.rs",
        "main",
        "current_dir",
        "",
        Reason::ParentSet,
    ),
    read(
        "crates/library/tests/worker.rs",
        "marker_path",
        "var_os",
        "BROKKR_MARKER_FIFO",
        Reason::Protocol,
    ),
    read(
        "crates/library/tests/process.rs",
        "fixture",
        "var",
        "PBFHOGG_LIBRARY_FIXTURE",
        Reason::ParentSet,
    ),
    read(
        "crates/library/src/main.rs",
        "boundary",
        "var_os",
        "BROKKR_USER_CONFIG",
        Reason::Protocol,
    ),
    read(
        "crates/library/src/main.rs",
        "boundary",
        "var_os",
        "XDG_CONFIG_HOME",
        Reason::Protocol,
    ),
    read(
        "crates/library/src/main.rs",
        "boundary",
        "var_os",
        "HOME",
        Reason::Protocol,
    ),
    read(
        "crates/library/src/main.rs",
        "boundary",
        "var_os",
        "BROKKR_WITH_EXE",
        Reason::Protocol,
    ),
    read(
        "crates/lib/tests/common/skip.rs",
        "requirement",
        "var",
        "PBFHOGG_TEST_REQUIRE",
        Reason::RunnerRequirement,
    ),
    read(
        "crates/lib/tests/common/oracle.rs",
        "executable",
        "var_os",
        "PATH",
        Reason::HostTool,
    ),
    // Relative PATH entries resolve against the parent's directory.
    read(
        "crates/lib/tests/common/oracle.rs",
        "executable",
        "current_dir",
        "",
        Reason::HostTool,
    ),
    read(
        "crates/test-scratch/src/hang.rs",
        "tests::expiry_probe",
        "var_os",
        "PBFHOGG_HANG_BOUND_PROBE",
        Reason::ParentSet,
    ),
    read(
        "crates/harness/tests/nofile.rs",
        "restricted_child",
        "var_os",
        "PBFHOGG_NOFILE_CONTRACT_CHILD",
        Reason::ParentSet,
    ),
    read(
        "crates/cli/tests/invoker.rs",
        "tests::gated_child_probe",
        "var_os",
        "PBFHOGG_TEST_GATED_PROBE",
        Reason::ParentSet,
    ),
    read(
        "crates/cli/tests/guard.rs",
        "tests::cases::lease",
        "var",
        "PBFHOGG_TEST_LEASE_FD",
        Reason::ParentSet,
    ),
    read(
        "crates/cli/tests/guard.rs",
        "tests::cases::probe",
        "var",
        "PBFHOGG_TEST_GUARD_PROBE",
        Reason::ParentSet,
    ),
    read(
        "crates/cli/src/host.rs",
        "cli_resources",
        "var_os",
        "PBFHOGG_TEST_RESOURCES",
        Reason::Protocol,
    ),
    read(
        "crates/cli/src/host.rs",
        "cli_invocation",
        "var_os",
        "BROKKR_MARKER_FIFO",
        Reason::Protocol,
    ),
    read(
        "crates/cli/src/lib.rs",
        "run",
        "var_os",
        "HOTPATH_OUTPUT_PATH",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::announce_fd",
        "var",
        "PBFHOGG_TEST_ANNOUNCE_FD",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm",
        "var_os",
        "PBFHOGG_TEST_CANCEL_COUNT",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm",
        "var_os",
        "PBFHOGG_TEST_CANCEL_PAUSE",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_publication",
        "var_os",
        "PBFHOGG_TEST_PUBLISH_FAULTS",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_publication",
        "var_os",
        "PBFHOGG_TEST_PUBLISH_STEPS",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_publication",
        "var_os",
        "PBFHOGG_TEST_PUBLISH_PAUSE",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_input_replacement",
        "var_os",
        "PBFHOGG_TEST_FORCE_PIN_PATH_REOPEN",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_input_replacement",
        "var_os",
        "PBFHOGG_TEST_REPLACE_INPUT",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_input_replacement",
        "var_os",
        "PBFHOGG_TEST_REPLACE_WITH",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_input_replacement",
        "var_os",
        "PBFHOGG_TEST_REPLACE_AFTER",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_input_replacement",
        "var_os",
        "PBFHOGG_TEST_REPLACE_REPORT",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::arm_merge_changes",
        "var_os",
        "PBFHOGG_TEST_MERGE_CHANGES_SPILL_FIRST",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::write_replacement_report",
        "var_os",
        "PBFHOGG_TEST_REPLACE_REPORT",
        Reason::Protocol,
    ),
    read(
        SIGNALS,
        "test_hooks::write_count",
        "var_os",
        "PBFHOGG_TEST_CANCEL_COUNT",
        Reason::Protocol,
    ),
    read(
        "crates/harness/src/runner.rs",
        "fifo_from_env",
        "var_os",
        "BROKKR_MARKER_FIFO",
        Reason::Protocol,
    ),
    read(
        "crates/harness/src/runner.rs",
        "profile_from_env",
        "var_os",
        "HOTPATH_OUTPUT_PATH",
        Reason::Protocol,
    ),
    read(
        "crates/bench/src/host.rs",
        "invocation",
        "var_os",
        "BROKKR_MARKER_FIFO",
        Reason::Protocol,
    ),
    read(
        "crates/bench/src/main.rs",
        "main",
        "var_os",
        "HOTPATH_OUTPUT_PATH",
        Reason::Protocol,
    ),
];

/// `std::env` names refused wherever they are spelled.
const REFUSED_STD: &[&str] = &[
    "set_var",
    "remove_var",
    "set_current_dir",
    "vars",
    "vars_os",
    "temp_dir",
    "home_dir",
];

/// `std::env` reads admitted at an inventory entry.
const READ_OPS: &[&str] = &["var", "var_os", "current_dir"];

/// libc names refused after `libc::` or in a `use libc` statement.
const REFUSED_LIBC: &[&str] = &[
    "getenv",
    "secure_getenv",
    "setenv",
    "unsetenv",
    "putenv",
    "clearenv",
    "environ",
];

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found: Vec<String> = super::require_files(corpus, "crates/", "environment")
        .into_iter()
        .collect();
    found.extend(check(corpus, READS));
    found
}

/// Every finding over `corpus` against the inventory `reads`.
pub fn check(corpus: &Corpus, reads: &[Read]) -> Vec<String> {
    let mut found = Vec::new();
    let mut seen = vec![0_usize; reads.len()];
    for (path, source) in corpus.under("crates/") {
        let file = match syn::parse_file(&source.text) {
            Ok(file) => file,
            Err(e) => {
                found.push(format!(
                    "{path}:{}: does not parse: {e}",
                    e.span().start().line
                ));
                continue;
            }
        };
        let owners = Owners::of(source, &file);
        let code = source.code();
        let (import_findings, statements) = imports(path, source, &code);
        found.extend(import_findings);
        // An import is judged as an import, not again as an access.
        for i in (0..code.len()).filter(|i| !statements.iter().any(|r| r.contains(i))) {
            let file = Judged {
                path,
                source,
                code: &code,
                owners: &owners,
            };
            if let Some(error) = access(&file, i, reads, &mut seen) {
                found.push(error);
            }
        }
    }
    for (entry, count) in reads.iter().zip(&seen) {
        if *count != entry.count {
            found.push(format!(
                "{}: inventoried env::{}({:?}) in {} found {count} times, expected {}",
                entry.file, entry.op, entry.key, entry.owner, entry.count
            ));
        }
    }
    found
}

fn word_at(source: &Source, code: &[Tok], i: usize) -> Option<String> {
    code.get(i).and_then(|t| source.name(t)).map(str::to_owned)
}

fn punct_at(source: &Source, code: &[Tok], i: usize, p: &str) -> bool {
    code.get(i).is_some_and(|t| source.is_punct(t, p))
}

/// `NAME ::` ending just before `i`, so `code[i]` follows `NAME::`.
fn after(source: &Source, code: &[Tok], i: usize, name: &str) -> bool {
    i >= 3
        && punct_at(source, code, i - 1, ":")
        && punct_at(source, code, i - 2, ":")
        && word_at(source, code, i - 3).as_deref() == Some(name)
}

/// A finding for the access at token `i`, if it is one: a refused name, or
/// a read that is not an inventoried direct call.
fn access(file: &Judged<'_>, i: usize, reads: &[Read], seen: &mut [usize]) -> Option<String> {
    let (path, source, code) = (file.path, file.source, file.code);
    let name = word_at(source, code, i)?;
    let line = code[i].line;
    if after(source, code, i, "libc") && REFUSED_LIBC.contains(&name.as_str()) {
        return Some(format!(
            "{path}:{line}: libc::{name} touches the process environment"
        ));
    }
    if !after(source, code, i, "env") {
        return None;
    }
    if REFUSED_STD.contains(&name.as_str()) {
        return Some(format!(
            "{path}:{line}: env::{name} mutates, enumerates or indirectly reads the \
             process environment; a test builds its inputs, and a child's environment \
             is set on its Command"
        ));
    }
    if !READ_OPS.contains(&name.as_str()) {
        return None;
    }
    let owner = file.owners.owner(code[i].start);
    let Some(key) = key_of(file, i, &name) else {
        return Some(format!(
            "{path}:{line}: env::{name} in `{owner}` is not a direct call with a \
             string literal key"
        ));
    };
    match reads
        .iter()
        .position(|r| r.file == path && r.owner == owner && r.op == name && r.key == key)
    {
        Some(k) => {
            seen[k] += 1;
            None
        }
        None => Some(format!(
            "{path}:{line}: env::{name}({key:?}) in `{owner}` is not an inventoried read; \
             tests never read the runner's environment"
        )),
    }
}

/// The key a read at `i` names: empty for `current_dir()`, else its one
/// argument's decoded string literal. A constant is not resolved: an import
/// (a glob included) or a local binding could shadow it, and the literal is
/// the one spelling whose value the rule can read.
fn key_of(file: &Judged<'_>, i: usize, op: &str) -> Option<String> {
    let (source, code) = (file.source, file.code);
    if !punct_at(source, code, i + 1, "(") {
        return None;
    }
    if op == "current_dir" {
        return punct_at(source, code, i + 2, ")").then(String::new);
    }
    if !punct_at(source, code, i + 3, ")") {
        return None;
    }
    let arg = code.get(i + 2)?;
    (arg.kind == Kind::Str)
        .then(|| decoded(source.text_of(arg)))
        .flatten()
}

/// The file being judged.
struct Judged<'a> {
    path: &'a str,
    source: &'a Source,
    code: &'a [Tok],
    owners: &'a Owners,
}

/// Import findings: a `use` or `extern crate` statement naming `env`, a
/// refused libc name, or aliasing or globbing `std` or `libc` themselves;
/// and the token ranges of every such statement.
fn imports(
    path: &str,
    source: &Source,
    code: &[Tok],
) -> (Vec<String>, Vec<std::ops::Range<usize>>) {
    let mut found = Vec::new();
    let mut statements = Vec::new();
    let mut i = 0;
    while i < code.len() {
        let starts = source.is_word(&code[i], "use")
            || source.is_word(&code[i], "extern")
                && code.get(i + 1).is_some_and(|t| source.is_word(t, "crate"));
        if !starts {
            i += 1;
            continue;
        }
        let end = (i..code.len())
            .find(|&k| source.is_punct(&code[k], ";"))
            .unwrap_or(code.len());
        if let Some(reason) = import_finding(source, &code[i..end]) {
            found.push(format!("{path}:{}: {reason}", code[i].line));
        }
        statements.push(i..end);
        i = end + 1;
    }
    (found, statements)
}

fn import_finding(source: &Source, stmt: &[Tok]) -> Option<String> {
    // For each open brace, whether the group's root is `std` or `libc`.
    let mut groups: Vec<bool> = Vec::new();
    let mut in_libc = false;
    for (k, tok) in stmt.iter().enumerate() {
        let name = source.name(tok);
        let root = |name: Option<&str>| matches!(name, Some("std" | "libc"));
        if root(name) {
            in_libc |= name == Some("libc");
            if stmt.get(k + 1).is_some_and(|t| source.is_word(t, "as")) {
                return Some(format!("`{} as` aliases a crate root", name.unwrap_or("")));
            }
        }
        if name == Some("env") {
            return Some(
                "a `use` names `env`; spell the environment by path, `std::env::NAME`".to_owned(),
            );
        }
        if in_libc && name.is_some_and(|n| REFUSED_LIBC.contains(&n)) {
            return Some(format!(
                "`use libc` imports {}, which touches the process environment",
                name.unwrap_or("")
            ));
        }
        let rooted_here = k >= 3
            && source.is_punct(&stmt[k - 1], ":")
            && source.is_punct(&stmt[k - 2], ":")
            && root(source.name(&stmt[k - 3]));
        if source.is_punct(tok, "{") {
            groups.push(rooted_here);
        } else if source.is_punct(tok, "}") {
            groups.pop();
        } else if source.is_punct(tok, "*") && (rooted_here || groups.last() == Some(&true)) {
            return Some("a glob of `std` or `libc` itself".to_owned());
        } else if source.is_word(tok, "self")
            && groups.last() == Some(&true)
            && stmt.get(k + 1).is_some_and(|t| source.is_word(t, "as"))
        {
            return Some("`self as` aliases a crate root".to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Read, Reason, check};
    use crate::Corpus;

    const FILE: &str = "crates/x/tests/t.rs";

    const READS: &[Read] = &[
        Read {
            file: FILE,
            owner: "gate",
            op: "var_os",
            key: "GATE",
            count: 1,
            reason: Reason::ParentSet,
        },
        Read {
            file: FILE,
            owner: "tests::probe",
            op: "var",
            key: "PROBE_VALUE",
            count: 1,
            reason: Reason::ParentSet,
        },
        Read {
            file: FILE,
            owner: "cwd",
            op: "current_dir",
            key: "",
            count: 1,
            reason: Reason::HostTool,
        },
    ];

    const BASE: &str = r#"
        fn gate() -> bool { std::env::var_os("GATE").is_some() }
        mod tests {
            fn probe() { let _ = std::env::var(r"PROBE_VALUE"); }
        }
        fn cwd() { let _ = ::std::env::current_dir(); }
    "#;

    fn findings(extra: &str) -> Vec<String> {
        let text = format!("{BASE}{extra}");
        check(&Corpus::from_sources([(FILE, text.as_str())]), READS)
    }

    fn replaced(from: &str, to: &str) -> Vec<String> {
        assert!(BASE.contains(from), "{from}");
        let text = BASE.replace(from, to);
        check(&Corpus::from_sources([(FILE, text.as_str())]), READS)
    }

    #[test]
    fn the_inventoried_base_passes() {
        assert_eq!(findings(""), Vec::<String>::new());
    }

    #[test]
    fn mutation_enumeration_and_libc_fire_anywhere() {
        for extra in [
            "fn f() { unsafe { std::env::set_var(\"A\", \"1\"); } }",
            "fn f() { unsafe { std::env::remove_var(\"A\"); } }",
            "fn f() { std::env::set_current_dir(\"/\"); }",
            "fn f() { for _ in std::env::vars() {} }",
            "fn f() { let _ = std::env::vars_os; }",
            "fn f() { std::env::temp_dir(); }",
            "fn f() { std::env::home_dir(); }",
            "fn f() { unsafe { libc::getenv(p); } }",
            "fn f() { unsafe { libc::setenv(a, b, 1); } }",
            "fn f() { unsafe { libc::clearenv(); } }",
            "use libc::{getpid, getenv};",
        ] {
            assert_eq!(findings(extra).len(), 1, "{extra}: {:?}", findings(extra));
        }
    }

    #[test]
    fn reads_are_inventoried_by_owner_and_key() {
        for extra in [
            "fn f() { std::env::var_os(\"HOME\"); }",
            "fn gate2() { std::env::var_os(\"GATE\"); }",
            "fn f() { std::env::var(\"GATE\"); }",
        ] {
            assert_eq!(findings(extra).len(), 1, "{extra}: {:?}", findings(extra));
        }
        // The same key in another owner, and the inventoried one gone.
        assert_eq!(replaced("fn gate()", "fn other()").len(), 2);
        // A second read of the inventoried key in its owner exceeds the count.
        let found = replaced(
            "std::env::var_os(\"GATE\").is_some()",
            "std::env::var_os(\"GATE\").is_some() && std::env::var_os(\"GATE\").is_some()",
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn the_key_is_the_decoded_literal() {
        // A raw literal reads as its value; another value is another key.
        assert_eq!(replaced("r\"PROBE_VALUE\"", "\"PROBE\\x5fVALUE\"").len(), 0);
        assert_eq!(replaced("r\"PROBE_VALUE\"", "\"HOME\"").len(), 2);
        // A constant key is refused: an import, glob or local could shadow it.
        let found = replaced(
            "let _ = std::env::var(r\"PROBE_VALUE\");",
            "const PROBE: &str = \"PROBE_VALUE\"; let _ = std::env::var(PROBE);",
        );
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn a_read_is_a_direct_call() {
        for (from, to) in [
            (
                "std::env::var_os(\"GATE\").is_some()",
                "{ let read = std::env::var_os; read(\"GATE\").is_some() }",
            ),
            (
                "std::env::var_os(\"GATE\")",
                "std::env::var_os(format!(\"GA{}\", \"TE\"))",
            ),
            ("std::env::var_os(\"GATE\")", "std::env::var_os(key)"),
            ("::std::env::current_dir()", "::std::env::current_dir(x)"),
        ] {
            assert!(!replaced(from, to).is_empty(), "{to}");
        }
    }

    #[test]
    fn imports_cannot_alias_the_environment() {
        for extra in [
            "use std::env;",
            "use std::env::var_os;",
            "use std::{env::{self, var_os as read}};",
            "use std::env::*;",
            "use std as platform;",
            "use ::std as platform;",
            "use std::*;",
            "use std::{io, *};",
            "use std::{self as platform};",
            "extern crate std as platform;",
            "use libc as c;",
            "use libc::*;",
            "use r#std::r#env;",
        ] {
            assert_eq!(findings(extra).len(), 1, "{extra}: {:?}", findings(extra));
        }
        for extra in [
            "use std::io::Write as _;",
            "use std::io::{self, Read as _};",
            "use std::os::unix::fs::*;",
            "use libc::getpid;",
            "fn f(env: &[u8]) { let _ = env.len(); }",
            "fn f(cmd: &mut C) { cmd.env(\"K\", \"V\").env_clear(); }",
            "fn f() { let _ = env!(\"CARGO_PKG_VERSION\"); }",
            "fn f() { let _ = std::env::current_exe(); let _ = std::env::args_os(); }",
            "// std::env::set_var\nfn g() { let _ = \"std::env::var\"; }",
        ] {
            assert!(findings(extra).is_empty(), "{extra}: {:?}", findings(extra));
        }
    }

    #[test]
    fn a_file_that_does_not_parse_fails() {
        assert_eq!(findings("fn broken( {").len(), 4);
    }
}
