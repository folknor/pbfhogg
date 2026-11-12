# Verification findings

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
```

Findings are as reported by the hunters and have not been verified. Hunters read
source; costs are derived from the code unless an entry says measured. "Reported
by" records which scopes raised a finding and is a fact about the reports, not a
verdict on the code. Entries are in no order of importance.
## VER-130 - The descriptor gate's charges are asserted, not observed

Descriptor-heavy library tests take a ticket from `crates/lib/tests/common/fd_gate.rs`
before building anything, and light hosts grant too little for an external join.
Unobserved: the per-run charge (`TEST_HEAVY_RUN_FDS`) and the light reserve
(`TEST_LIGHT_FD_RESERVE`) are asserted, never compared with a run's real descriptor
high-water mark; a ticket's units bound its simultaneous runs only by the scenario's
construction; tests that build their own `host::FdPool` hold a ticket by convention.
Residue: measure the high-water marks (a `/proc/self/fd` count at the gate's release
would do) and check them against the charges, if it pays.

## VER-009 - The static inventory cannot see synthesized state

`crates/source-policy/src/rules/statics.rs` inventories every library static with
counted, audited exceptions and presence checks for protected files, with its fixtures
beside it. Unenforced: statics in synthesized macro output,
changes within audited initializers, and an architectural rule that `FaultPlan` is
constructed only through the invocation.

## VER-010 - The probe-lock and counter-emission guards are fixed-shape, not AST

The probe open now goes through `open_probe_lock` in `crates/lib/src/scratch.rs`,
tested for missing-file preservation, and counter emission lives in
`finish_invocation` in `crates/cli/src/lib.rs`, tested on success, refusal and
cancellation. The guard in `crates/source-policy/src/rules/lifecycle.rs` checks a
fixed token shape of each; it does not prove arbitrary Rust control flow. syn is
now in that crate, so an AST check would not need a new parser. Residue: a general
AST check, if it pays.

## VER-014 - Empty textlint globs and stale exclusions are only audit-enforced

Every rule in `brokkr.d/lints.toml` now has a `must_scan` sentinel, and the old coverage
script and its migration manifest are retired. brokkr still lets an empty `paths` glob or
a stale exclusion pass an ordinary check; only its audit flags them. Brokkr side: bz
brokkr#157.

## VER-018 - The gate's backend requirement checks refusals, not execution

The gate requires `direct-io`, `io-uring` and `promise-guard`. Residue: the
requirement checks refusal paths, not positive execution, so a cell that stops
exercising its backend without consulting a predicate still passes; a green gate need
not have exercised the io_uring writer's smaller fallback pools or its retry (a host
with a large limit registers the full pool first time); the CLI predicates read the
refused label from stderr text, which exit code 3 authenticates as a refusal but not
as that label; and concurrent io_uring test writers share the per-user memlock
allowance with no cross-process admission, so on a low-limit host a contended
registration can fail the gate falsely. Not yet measured: the fallback pools' wall
against the full pool on planet (needs a raised limit, which this host lacks).

## VER-026 - CLI tests wait on child bounds in sequence

Every test time bound lives in `crates/lib/src/tunables.rs` ("Test time bounds") and
is const-asserted below `TEST_RUNNER_WATCHDOG`, brokkr's fixed 20 s per-test
watchdog, which the maintainer confirms is always 20 s. Library hang waits now carry
one fail-stop `HangBound` over the whole scenario, but tests outside them still wait
on bounds in sequence: a CLI test that runs several children, each under `TEST_CHILD_TIMEOUT`
(10 s), can reach the watchdog unnamed, and CLI tests cannot name `TEST_HANG_BOUND`
through the test-import audit (`crates/source-policy/src/rules/test_imports.rs`).

## VER-137 - Literal test bounds and a handle joined only on success

Found while closing the hang-helper work. Failure bounds still written as literals
outside "Test time bounds": `read/decompress.rs` (5 s; its worker handle is dropped
unjoined on expiry), the 5 s deadline loops in `ordered_pool.rs` and
`coord_payloads.rs`, the `read/pipeline.rs` stalled-transform and teardown tests (5 s,
10 s), `crates/cli/src/signals.rs` `BOUND` (10 s), the `fault_inspect_input.rs` and
`fault_degrade_input.rs` feeders (10 s), and the `cli_output_contract.rs` loops and
`READY_TIMEOUT`. `crates/lib/tests/seal.rs` joins its two spawned threads only on the
success path - a named handle dropped on an assertion failure, which the
detached-threads source rule (statement and `let _` discards only) cannot see.
`capabilities.rs` bounds a thread rendezvous with `TEST_CHILD_TIMEOUT`.

## VER-033 - Twelve library commands have no cancellation scenario

`crates/lib/tests/cancel.rs` keeps a registry of all 19 public library command modules,
compared mechanically against the module declarations: seven are exercised, twelve are
excluded explicitly, and checkpoint replays are capped at 64 per scenario. Scenarios for
the excluded twelve, and a comparison against the CLI command inventory, are not done.

## VER-133 - Test-local metadata readers and PBF writers bypass the shared oracle

Found while closing the shared-oracle work.

`common::read_normalized` now decodes independently and refuses what it cannot
resolve, but many test files keep their own lossy projections outside
`crates/lib/tests/common`, where the `test-oracles-no-error-to-empty` text rule
does not reach: `info_meta` and `ordered_elements` in `crates/cli/tests/cli_cat.rs`
(version-gated, `unwrap_or(0)`, `user().unwrap_or("")`), `info_meta` and
`assert_roundtrip` in `crates/lib/tests/proptests.rs` (seconds, `BTreeMap` tags),
`RawNodeMeta` in `crates/lib/tests/getid.rs` and `read_raw_node_meta` in
`crates/cli/tests/cli_sort.rs` (`unwrap_or("")`, `and_then(Result::ok)`),
`read_nodes_with_metadata` in `crates/cli/tests/cli_time_filter.rs` (seconds,
`version().unwrap_or(0)`), the member reader in
`crates/cli/tests/cli_apply_changes_osmium.rs` (`_ => "unknown"`), and the sorted
tags of `crates/lib/tests/read_paths.rs`. About 110 `PbfWriter::new` or `to_path`
loops across some 45 test files (cli_digest's `write_full`, cli_time_filter's
`write_node_blocks` among them) repeat the fixture traversal instead of
`encode_fixture_blocks` plus `frame_fixture_blocks`.

Fix: route each through `read_normalized` or the model, or state why a projection
is deliberately narrower; widen the text rule to the test trees once they are clean.

## VER-134 - Other library fixtures still pay production thresholds

Found while shrinking the diff, apply-changes and merge-changes fixtures.

`crates/lib/tests/merge_pbf.rs` sizes fixtures against crate-private sweep
constants (`PLANET_BLOB_ELEMENTS = 228_000` in
`a_planet_density_self_merge_streams_its_run_in_bounded_chunks` and
`sorted_views_are_charged_to_the_run`, `VERSIONS = 100_000` in
`a_many_version_group_ships_block_by_block`), and
`crates/lib/tests/geocode_index.rs` builds 65,536-node inputs at three sites,
all at tier 1. Not measured per test.

Fix: the same rule as the diff planner, create-run and merge-changes batching
work: a narrow parameter or test-hooks override at the owning mechanism,
small tier-1 fixtures, one tier-3 test of the production value.

## VER-071 - The verbose sequence budget has no registered timing bench

The ignored timing probe in `commands/diff/seq.rs` is removed; its work-limit assertions
stay. Timing belongs in a registered bench: synthetic modified-way pairs with 30,000
refs (disjoint ranges, and every 60th ref changed), measuring `diff --verbose`, requiring
exit 1 and checking replacement versus itemized output, with fixture preparation outside
measurement. Not written: no existing command or bench helper produces those
pairs untimed, so the bench needs a fixture producer first.



## VER-078 - CLI children use the real machine

Reported by cli-tests.

`cli_host` now takes resolved resources and `CliInvoker` gives every child fixed sizing inputs
(`PBFHOGG_TEST_RESOURCES`, a `test-hooks` hook, plus `RAYON_NUM_THREADS`). Residue: children built
without `test-hooks` (the `consumer` and `musl` sweeps) ignore the resource variable and still size
resource-derived pools and route altw auto and check locations on the runner's real cores and
memory. Release builds cannot take a private override, so closing this needs either a different
execution boundary for those sweeps or acceptance that they are process-integration coverage only.

## VER-090 - The requirement-table check is convention-bound

`required` in `crates/harness/src/describe.rs` now refuses anything outside its convention
(exactly one root table, exact line shape, no duplicate ids, and any pbfhogg or library key
or nested include in an included file), and the authoritative composed check is
`brokkr library --all`. It still does not parse composed TOML; residue only if a
composed-configuration parse is ever wanted, which needs a brokkr configuration export
brokkr does not offer.

## VER-098 - The instrumented CLI and release backends are not exercised

Reported by library-harness, check-machinery.

The gate now runs a `release` sweep (default features, no hook cfg), a profiling lane over the
harness, and lint-only profiling and mixed-backend shapes. Two gaps remain. The CLI is compiled
instrumented but never run so: its `HotpathGuardBuilder` in `run()` is unconditional, so an
instrumented CLI's quiet unprofiled run, its report placement and validity are checked by nothing but
`brokkr bench --hotpath` use. And the O_DIRECT and io_uring backends never run in release.

## VER-104 - The document checker is not verified against renderer output

`scripts/check_doc_links.py` is tested against 18 fixtures in `scripts/doc_links_fixtures/`,
twelve derived from the installed VitePress source and six GitHub contract cases. Full
renderer parity and sidebar route and base behaviour remain unverified.


## VER-107 - The musl sweeps assume an x86_64 host

Reported by check-machinery.

Both executable musl sweeps in `brokkr.d/checks.toml` hard-code `x86_64-unknown-linux-musl`,
now with comments and a host restriction stated in `reference/testing.md`. On aarch64 they
would be cross-builds with foreign executables. Residue is brokkr-side check planning:
resolve a runnable target from host capability, or refuse an unexecutable target before
building. Filed as bz brokkr#170.

## VER-112 - Orphaned expectation keys are not rejected

The six integrity and diff benches take `--require-expectation yes|no` through the shared
`selected` helper in `benches/lib/expect.lua`. Still open: configuration validation rejecting
orphaned digest-pair and OSC expectation keys (bz brokkr#161 is closed without a
close message; whether it landed orphan-key validation is unverified), and an
independent oracle for the correct answer.

## VER-113 - dedupe-pair does not prove its copy path

`diff-unindexed.lua` now probes both fronts before measuring and fails on indexed input.
`dedupe-pair.lua` still cannot require passthrough or coalesced copying: that needs brokkr's
Lua contract to expose child counters (`cat_dedupe_blobs_passthrough`,
`cat_dedupe_copy_range_coalesced`). Proving complete indexdata absence needs an
indexed-blob count from `inspect`; `inspect --get indexed=false` includes partially
indexed inputs.

## VER-118 - Three osmium comparison rows have no equivalence check

`vs-osmium.lua --validate-only yes` decodes both tools' PBF outputs independently through
osmium OPL with way locations and compares them, for nine rows; the three text-report rows
are refused. Complete-ways and node-retention behaviour are checked only at their
defaults, because pbfhogg has no affirmative flag for them.

## VER-122 - Verify's equality oracle is narrower than "identical" and shares the subject's code

Reported by bench-verify.

`VerifyHarness::diff_pbfs` in brokkr uses pbfhogg diff and `verify_sort` prints "identical", while
`crates/lib/src/content.rs` excludes metadata, inline way locations, headers and encoding. A shared decoder
defect can affect both sides.

Fix: an equality policy per check and an independent decoder for promised fields. Full digest strengthens
same-project checks but is no independent oracle. Enforcement: negative controls altering each field class.
Brokkr side filed as bz brokkr#156.

## VER-123 - Verify uses the build output directory as its workspace

Reported by bench-verify.

`VerifyHarness` places outputs under `<target>/verify` and `subdir` recursively replaces each check's fixed
directory. Cargo owns `target`, so these PBFs and reports vanish with cleanup and overwrite earlier evidence.

Fix: a brokkr-owned per-run directory under configured scratch and a separate retained-evidence area.
Enforcement: directory capability types.
brokkr declined this (bz brokkr#154, wontfix); brokkr verify is to be removed and
reimplemented in pbfhogg, which must meet the requirement tracked under VER-127.

## VER-124 - Optional context tools add cost without a verdict

Reported by bench-verify.

`verify all` provisions osmosis, and `run_context_tools` in brokkr runs osmosis and osmconvert for context,
whose failures do not change the verdict.

Fix: an explicit diagnostic option. Enforcement: a declarative step graph rejecting unused outputs.
Brokkr side filed as bz brokkr#155.


## VER-127 - Verify workspace requirement on the pbfhogg verify reimplementation

The verify workspace (see VER-123; brokkr side brokkr#154, closed wontfix).
brokkr verify is to be removed and reimplemented in pbfhogg, so this is a
requirement on that implementation: its outputs go to an owned per-run scratch
directory, with any retained evidence kept separately, never under `target/`.
The rest of the test-filesystem work is done; its settled design is stated
in `reference/testing.md` "Test scratch" and in the leaf crate
`crates/test-scratch`.
