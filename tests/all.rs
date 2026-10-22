//! pbfhogg's integration tests, as one test binary.
//!
//! One binary rather than one per file: every harness links the whole
//! library again, each check sweep links every harness, and a focused
//! `brokkr test NAME` lists every harness one at a time to find one test.
//! The root `Cargo.toml` sets `autotests = false`, so a file in `tests/`
//! that is not declared here (or as its own `[[test]]` target) is never
//! compiled, and nothing reports it.
//!
//! A test's full libtest name is now `<file module>::<path inside the file>`
//! (`cli_sort::platform::...`). The tier submodules (`tier2`, `tier3`,
//! `platform`, `serial`) keep matching brokkr's substring filters because
//! the module prefix only prepends.
//!
//! Every module here shares one process with every other, concurrently. The
//! process-wide state they share: the global rayon pool; and
//! `RLIMIT_NOFILE`, raised once by `common::test_host()`. A test that needs
//! any of this state to itself gets its own `[[test]]` target in
//! `Cargo.toml` with the reason beside it.
//!
//! The library's process-global fault hooks cannot be armed from here at
//! all: they exist only under `static-test-hooks`, and the `cfg` below
//! empties this whole binary whenever that feature is on. A build that has
//! the statics therefore has no tests here, and a build that has tests here
//! has no statics to name. A test that arms one gets its own `[[test]]`
//! target requiring `static-test-hooks` - see the `fault_*` targets.
//! Per-instance hooks (`test-hooks`) are fine here.
#![cfg(not(feature = "static-test-hooks"))]

mod common;

// Gated on `commands` because they reach modules that feature gates
// (`check`, `export`, `extract`, `inspect`'s JSON output, the geocode
// builder, and `serde_json`, a `commands`-only dependency), so the
// `minimal` sweep drops them rather than failing to compile.
#[cfg(feature = "commands")]
mod check;
#[cfg(feature = "commands")]
mod cli_check_index;
#[cfg(feature = "commands")]
mod cli_export;
#[cfg(feature = "commands")]
mod cli_extract;
#[cfg(feature = "commands")]
mod cli_output_guard;
#[cfg(feature = "commands")]
mod geocode_index;
#[cfg(feature = "commands")]
mod inspect;

mod cli_add_locations_to_ways;
mod cli_apply_changes;
mod cli_apply_changes_invariants;
mod cli_cat;
mod cli_defensive_input;
mod cli_degrade;
mod cli_derive_changes;
mod cli_diff;
mod cli_digest;
mod cli_exit_codes;
mod cli_inject_prepass;
mod cli_merge_changes;
mod cli_merge_join_trust;
mod cli_negative_id_invariants;
mod cli_repack;
mod cli_sort;
mod cli_tags_filter;
mod cli_time_filter;
mod cli_truncation_sweep;
mod corrupt_input;
mod edge_cases;
mod fault_apply_changes;
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
mod roundtrip_real;
mod tags_count;
mod tags_filter_osc;

#[cfg(all(feature = "commands", target_os = "linux", target_env = "gnu"))]
mod cli_host_identity;

mod host_contract;
