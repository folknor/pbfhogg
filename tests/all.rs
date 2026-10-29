//! pbfhogg's integration tests, as one test binary.
//!
//! One binary rather than one per file: every harness links the whole
//! library again, each check sweep links every harness, and a focused
//! `brokkr test NAME` lists every harness one at a time to find one test.
//! The root `Cargo.toml` sets `autotests = false`, so a file in `tests/`
//! that is not declared here (or as its own `[[test]]` target) is never
//! compiled, and nothing reports it.
//!
//! Nothing here drives the `pbfhogg` binary: those tests are the CLI
//! package's (`cli/tests/all.rs`), where cargo builds the binary with the
//! test build's features and hands its path.
//!
//! A test's full libtest name is now `<file module>::<path inside the file>`
//! (`host_contract::direct_read::...`). The tier submodules (`tier2`,
//! `tier3`, `serial`) keep matching brokkr's substring filters because
//! the module prefix only prepends.
//!
//! Every module here shares one process with every other, concurrently. The
//! process-wide state they share: the global rayon pool; and
//! `RLIMIT_NOFILE`, raised once by `common::test_host()`. A test that needs
//! any of this state to itself gets its own `[[test]]` target in
//! `Cargo.toml` with the reason beside it.
//!
//! Fault tests live here too: the library's fault hooks
//! (`pbfhogg::test_hooks`) ride the token each run is handed, so an armed
//! fault reaches only the run that armed it. The `fault_*` modules exist only
//! under `test-hooks`.

// `allow-expect-in-tests` (clippy.toml) exempts only `#[test]` functions and
// `#[cfg(test)]` modules, not the helpers of an integration-test crate, which
// is test code throughout.
#![allow(
    clippy::expect_used,
    reason = "an integration-test crate; a failed expect is a test failure"
)]

mod common;

// Gated on `commands` because they reach modules that feature gates
// (`check`, `export`, `extract`, `inspect`'s JSON output, the geocode
// builder, and `serde_json`, a `commands`-only dependency), so the
// `minimal` sweep drops them rather than failing to compile.
#[cfg(feature = "commands")]
mod cancel;
#[cfg(feature = "commands")]
mod check;
#[cfg(feature = "commands")]
mod check_locations;
#[cfg(feature = "commands")]
mod geocode_index;
#[cfg(feature = "commands")]
mod inspect;

mod corrupt_input;
mod digest_input;
mod edge_cases;
#[cfg(feature = "test-hooks")]
mod fault_altw_input;
#[cfg(feature = "test-hooks")]
mod fault_altw_stage3;
#[cfg(feature = "test-hooks")]
mod fault_altw_stage4;
mod fault_apply_changes;
#[cfg(feature = "test-hooks")]
mod fault_cat_input;
#[cfg(all(feature = "test-hooks", feature = "commands"))]
mod fault_check_ids_input;
#[cfg(feature = "test-hooks")]
mod fault_degrade_input;
#[cfg(feature = "test-hooks")]
mod fault_derive_input;
#[cfg(feature = "test-hooks")]
mod fault_derive_parallel;
#[cfg(feature = "test-hooks")]
mod fault_diff_input;
#[cfg(feature = "test-hooks")]
mod fault_diff_parallel;
#[cfg(feature = "test-hooks")]
mod fault_digest_input;
#[cfg(all(feature = "test-hooks", feature = "commands"))]
mod fault_extract_grid;
#[cfg(all(feature = "test-hooks", feature = "commands"))]
mod fault_extract_input;
#[cfg(all(feature = "test-hooks", feature = "commands"))]
mod fault_geocode_input;
#[cfg(all(feature = "test-hooks", feature = "commands"))]
mod fault_geocode_pass3;
#[cfg(feature = "test-hooks")]
mod fault_getid_input;
#[cfg(feature = "test-hooks")]
mod fault_getparents_input;
#[cfg(feature = "test-hooks")]
mod fault_inspect_input;
#[cfg(feature = "test-hooks")]
mod fault_library_pools;
#[cfg(feature = "test-hooks")]
mod fault_parallel_writer;
#[cfg(feature = "test-hooks")]
mod fault_pipelined_writer;
#[cfg(feature = "test-hooks")]
mod fault_read_pipeline;
#[cfg(feature = "test-hooks")]
mod fault_renumber_input;
#[cfg(feature = "test-hooks")]
mod fault_repack_input;
#[cfg(feature = "test-hooks")]
mod fault_sort_input;
#[cfg(feature = "test-hooks")]
mod fault_tags_count_input;
#[cfg(feature = "test-hooks")]
mod fault_tags_filter_input;
#[cfg(feature = "test-hooks")]
mod fault_time_filter_input;
#[cfg(all(feature = "test-hooks", feature = "linux-io-uring"))]
mod fault_uring_writer;
mod fixture_helpers;
mod getid;
mod getparents;
mod merge_pbf;
mod non_indexed_parity;
mod osc_parser_parity;
mod proptests;
mod read_paths;
mod renumber_external;
mod roundtrip;
mod roundtrip_invariants;
#[cfg(feature = "commands")]
mod tags_count;
mod tags_filter_osc;

mod host_contract;
