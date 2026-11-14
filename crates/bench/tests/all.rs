//! pbfhogg-bench's integration tests, as one test binary: every test that
//! drives the instrument executable across its process boundary (its
//! command line, the environment brokkr hands it, its exit status and the
//! key=value block on stderr its scripts parse).
//!
//! Every child runs under the CLI's promise guard (`guard`, mounted from
//! `crates/cli/tests/guard.rs`), as the harness's do. `common` is the
//! library package's `crates/lib/tests/common`, mounted by path, for the
//! fixture writers and the shared scratch.

#![allow(
    clippy::expect_used,
    reason = "an integration-test crate; a failed expect is a test failure"
)]

#[path = "../../lib/tests/common/mod.rs"]
mod common;
#[cfg(target_os = "linux")]
#[path = "../../cli/tests/guard.rs"]
mod guard;

#[cfg(target_os = "linux")]
mod exe;
