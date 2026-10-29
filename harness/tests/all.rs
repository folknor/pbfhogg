//! pbfhogg-harness's integration tests, as one test binary: every test that
//! drives the harness executable across its process boundary (the command
//! line, the environment brokkr hands it, its exit status, stdout and
//! stderr, the artefacts it writes).
//!
//! Every child runs under the CLI's promise guard (`guard`, mounted from
//! `cli/tests/guard.rs` at the crate root so its control test keeps its
//! name): the harness inherits brokkr's lease descriptor as the CLI does,
//! and makes the same promise about it. Process policy (the descriptor
//! limit the harness raises) changes only in the children.
//!
//! `common` is the library package's `tests/common`, mounted by path, for
//! the fixture writers and the shared scratch.

#![allow(
    clippy::expect_used,
    reason = "an integration-test crate; a failed expect is a test failure"
)]

#[path = "../../tests/common/mod.rs"]
mod common;
#[cfg(target_os = "linux")]
#[path = "../../cli/tests/guard.rs"]
mod guard;

#[cfg(target_os = "linux")]
mod exe;
