//! pbfhogg-cli's integration tests, as one test binary: every test that
//! drives the `pbfhogg` binary.
//!
//! They live in this package, not the library's, because cargo builds the
//! binary for this package's integration tests with the test build's own
//! features, profile and target, and hands its path as
//! `CARGO_BIN_EXE_pbfhogg` (see `invoker::pbfhogg_bin`). A test in the root
//! package could only guess where some earlier build left a binary.
//!
//! One binary for the same reasons as the library's `crates/lib/tests/all.rs`, which
//! this mirrors: `autotests = false` in `crates/cli/Cargo.toml`, so a file in
//! `crates/cli/tests/` that is not declared here is never compiled; the
//! test-module audit in `crates/source-policy` reports such a file. A test's full libtest name is `<file module>::<path inside
//! the file>` (`cli_sort::backends::...`), so brokkr's tier filters keep
//! matching.
//!
//! Every module here shares one process with every other, concurrently,
//! and the inherited `RLIMIT_NOFILE`, which nothing here changes: the
//! spawned `pbfhogg` binaries raise their own. `common::test_host()` gives
//! each in-process host an executor pool of its own and fixed descriptor
//! grants, so only a test choosing `Executor::Ambient` reaches the global
//! rayon pool.
//!
//! `common` is the library package's `crates/lib/tests/common`, mounted by path, so its
//! `cfg(feature = ...)` gates read this package's features (the same names).

// `allow-expect-in-tests` (clippy.toml) exempts only `#[test]` functions and
// `#[cfg(test)]` modules, not the helpers of an integration-test crate, which
// is test code throughout.
#![allow(
    clippy::expect_used,
    reason = "an integration-test crate; a failed expect is a test failure"
)]

#[path = "../../lib/tests/common/mod.rs"]
mod common;
#[cfg(target_os = "linux")]
mod guard;
mod invoker;

mod cli;
mod cli_add_locations_to_ways;
mod cli_apply_changes;
mod cli_apply_changes_invariants;
mod cli_apply_changes_osmium;
mod cli_cat;
mod cli_check_index;
mod cli_check_locations;
mod cli_command_interval;
mod cli_defensive_input;
mod cli_degrade;
mod cli_derive_changes;
mod cli_diff;
mod cli_digest;
mod cli_error_naming;
mod cli_exit_codes;
mod cli_export;
mod cli_extract;
mod cli_flag_contract;
mod cli_inject_prepass;
mod cli_input_contract;
mod cli_merge_changes;
mod cli_merge_join_trust;
mod cli_negative_id_invariants;
mod cli_output_contract;
mod cli_output_guard;
mod cli_output_header;
mod cli_quiet;
mod cli_repack;
mod cli_scratch;
mod cli_sort;
mod cli_tags_count;
mod cli_tags_filter;
mod cli_time_filter;
mod cli_truncation_sweep;
