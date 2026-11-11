//! The one skip report, shared by every harness: integration tests reach it
//! as `common::skip`, the library's unit tests as `test_support::skip`, and
//! the promise guard (`crates/cli/tests/guard.rs`) mounts it on its own, so
//! this file depends on std alone.
//!
//! Two kinds of skip exist. [`skip`] announces a test, or a named portion of
//! one, that cannot run for an environmental reason (no osmium in PATH, no
//! unprivileged namespaces, a descriptor limit too low) and never fails.
//! A promised backend is abandoned only through [`fail_if_required`]: every
//! refusal predicate that authorizes dropping an O_DIRECT, io_uring or
//! promise-guard cell calls it on a match (or calls [`backend_unavailable`],
//! which also announces), so where the run requires that backend the test
//! fails instead of skipping. Predicates that only classify an expected
//! refusal (a test asserting the refusal itself) inspect the error directly
//! and never come here.
//!
//! The requirement is `PBFHOGG_TEST_REQUIRE`, a comma-separated list of
//! [`Backend::label`] values that the gate profile in `brokkr.d/checks.toml`
//! sets. Libtest has no skipped outcome and brokkr counts a returning test as
//! passed, so without it a mistaken platform refusal turns a backend test
//! into a passing skip. Portable runs leave it unset and skip as before. An
//! unknown token or a malformed value fails every test that consults it:
//! silently ignoring a misspelled backend would disable the enforcement.

// Each mount uses a different subset.
#![allow(dead_code)]

/// A backend the complete gate promises to execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// O_DIRECT reads and writes (`linux-direct-io`).
    DirectIo,
    /// The io_uring writer (`linux-io-uring`).
    IoUring,
    /// The seccomp promise guard every CLI test child runs under.
    PromiseGuard,
}

impl Backend {
    const ALL: [Self; 3] = [Self::DirectIo, Self::IoUring, Self::PromiseGuard];

    /// The token naming this backend in `PBFHOGG_TEST_REQUIRE`. The two I/O
    /// labels match the `feature` of the typed `PlatformUnsupported` refusal.
    pub fn label(self) -> &'static str {
        match self {
            Self::DirectIo => "direct-io",
            Self::IoUring => "io-uring",
            Self::PromiseGuard => "promise-guard",
        }
    }
}

/// The environment variable listing the backends this run must execute.
pub const REQUIRE_ENV: &str = "PBFHOGG_TEST_REQUIRE";

/// Report a test (or the named portion of one) that cannot run on this host
/// for an environmental reason. Never fails. The caller returns or drops that
/// portion itself. A promised backend's refusal is first checked by
/// [`fail_if_required`] (every refusal predicate does so), or goes through
/// [`backend_unavailable`].
pub fn skip(reason: impl std::fmt::Display) {
    eprintln!("Skipping: {reason}");
}

/// Record that a cell needing `cell` cannot execute, because of `reason`
/// (the typed refusal that stopped it, whose label may name another backend:
/// the io_uring writer also needs O_DIRECT). Fails the test when the run
/// requires `cell`; otherwise announces the skip, and the caller returns or
/// drops the cell itself.
pub fn backend_unavailable(cell: Backend, reason: impl std::fmt::Display) {
    fail_if_required(cell, &reason);
    eprintln!("Skipping: {} unavailable: {reason}", cell.label());
}

/// The enforcing half of [`backend_unavailable`], for a refusal predicate
/// whose caller announces the skip itself (with the route or portion it
/// drops) through [`skip`]: fails the test when the run requires `cell`.
pub fn fail_if_required(cell: Backend, reason: &dyn std::fmt::Display) {
    assert!(
        !required(cell),
        "{REQUIRE_ENV} requires {} and this cell could not execute: {reason}",
        cell.label()
    );
}

/// Whether this run must execute `backend`.
pub fn required(backend: Backend) -> bool {
    match requirement() {
        Ok(list) => list.contains(&backend),
        Err(message) => panic!("{message}"),
    }
}

fn requirement() -> &'static Result<Vec<Backend>, String> {
    static REQUIREMENT: std::sync::OnceLock<Result<Vec<Backend>, String>> =
        std::sync::OnceLock::new();
    REQUIREMENT.get_or_init(|| match std::env::var(REQUIRE_ENV) {
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(std::env::VarError::NotUnicode(value)) => {
            Err(format!("{REQUIRE_ENV} is not unicode: {value:?}"))
        }
        Ok(value) => parse_requirement(&value),
    })
}

fn parse_requirement(value: &str) -> Result<Vec<Backend>, String> {
    value
        .split(',')
        .map(|token| {
            Backend::ALL
                .into_iter()
                .find(|backend| backend.label() == token)
                .ok_or_else(|| {
                    format!(
                        "{REQUIRE_ENV}={value:?}: unknown backend {token:?}; known: {}",
                        Backend::ALL.map(Backend::label).join(",")
                    )
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Backend, parse_requirement};

    #[test]
    fn requirement_parses_every_label_and_refuses_anything_else() {
        assert_eq!(
            parse_requirement("direct-io,io-uring,promise-guard"),
            Ok(vec![
                Backend::DirectIo,
                Backend::IoUring,
                Backend::PromiseGuard
            ])
        );
        for malformed in [
            "",
            "io_uring",
            "direct-io,",
            " direct-io",
            "direct-io,,io-uring",
        ] {
            assert!(parse_requirement(malformed).is_err(), "{malformed:?}");
        }
    }
}
