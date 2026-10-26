//! pbfhogg-cli's integration tests, as one test binary: every test that
//! drives the `pbfhogg` binary.
//!
//! They live in this package, not the library's, because cargo builds the
//! binary for this package's integration tests with the test build's own
//! features, profile and target, and hands its path as
//! `CARGO_BIN_EXE_pbfhogg` (see `invoker::pbfhogg_bin`). A test in the root
//! package could only guess where some earlier build left a binary.
//!
//! One binary for the same reasons as the library's `tests/all.rs`, which
//! this mirrors: `autotests = false` in `cli/Cargo.toml`, so a file in
//! `cli/tests/` that is not declared here is never compiled, and nothing
//! reports it. A test's full libtest name is `<file module>::<path inside
//! the file>` (`cli_sort::platform::...`), so brokkr's tier filters keep
//! matching.
//!
//! Every module here shares one process with every other, concurrently: the
//! global rayon pool and `RLIMIT_NOFILE`, raised once by
//! `common::test_host()` and inherited by every spawned binary.
//!
//! `common` is the library package's `tests/common`, mounted by path, so its
//! `cfg(feature = ...)` gates read this package's features (the same names).

// `allow-expect-in-tests` (clippy.toml) exempts only `#[test]` functions and
// `#[cfg(test)]` modules, not the helpers of an integration-test crate, which
// is test code throughout.
#![allow(
    clippy::expect_used,
    reason = "an integration-test crate; a failed expect is a test failure"
)]

#[path = "../../tests/common/mod.rs"]
mod common;
mod invoker;

mod cli;
mod cli_add_locations_to_ways;
mod cli_apply_changes;
mod cli_apply_changes_invariants;
mod cli_cat;
mod cli_check_index;
mod cli_check_locations;
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
