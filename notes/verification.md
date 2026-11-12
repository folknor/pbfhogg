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
on bounds in sequence. One child is now one bound (`RunningCli` carries a single
deadline from spawn through `wait_for_file` and `wait`), but a CLI test that runs
several children, each under `TEST_CHILD_TIMEOUT` (10 s) - a row loop, or a run and
then a next run as in `a_second_signal_exits_at_once_and_the_next_run_reports_the_leftovers`
- can reach the watchdog unnamed, and CLI tests cannot name `TEST_HANG_BOUND`
through the test-import audit (`crates/source-policy/src/rules/test_imports.rs`).

## VER-139 - Test threads not yet checked for release before join

Found while closing the literal-bound and success-only-join work, which converted
`seal.rs`, `capabilities.rs`, the `read/reader.rs` budget and queue tests, the
`read/pipeline.rs` gate test and the FIFO feeders. Not yet read against the rule
(every thread a test starts is released and joined on every exit, under the test's
`HangBound`): `crates/lib/tests/fault_pipelined_writer.rs` (the `raw` writer thread is
declared after its `Cleanup` guard, so an assertion failure drops the handle before
the guard releases framing, detaching the thread), `crates/lib/tests/digest_input.rs`
FIFO writer, the `write/writer.rs`, `write/pipeline.rs` and `read/reader.rs` (the
spawn near the `par_map_reduce` tests) unit-test spawns,
`commands/apply_changes/window.rs` (`a_returned_drain_wakes_a_parked_scanner`, no
bound on its join) and `crates/cli/tests/guard.rs`. `crates/cli/tests/cli_sort.rs`
`sort_cross_validate_osmium` sets a literal 110 s child timeout, above the watchdog,
on an `#[ignore]` test run only through `brokkr test --timeout`.
`OwnedChild::signal` (`crates/lib/tests/common/process.rs`) still calls `kill` on the
numeric PID after the child was reaped by `has_exited`; it should refuse instead.
`seal.rs` and `crates/cli/src/signals.rs` join through `std::thread::scope`, which
does not wait for thread-local destructors; their threads hold none that matter, but
only lib unit tests can reach `crate::threads::scope`.

## VER-138 - Element order inside a mixed-field group is pbfhogg's, not the file's

Found while giving the shared oracle a wire-order view.

`BlockElementsIter` (`crates/lib/src/read/block.rs`) yields a PrimitiveGroup's
dense nodes first, then plain nodes, ways and relations; libosmium
(`decode_primitive_block_data`) and the format yield the group's fields in wire
order. They differ only inside a group that mixes element fields (a plain node
before a DenseNodes field, say). Plain `cat` re-encodes such a block "in element
order", so it would write the plain node after the dense ones; no fixture puts
plain before dense in one group, so `cli_cat.rs`'s order-preserving tests do not
see it, and `DEVIATIONS.md` does not record it. Unmeasured, and such groups are
malformed-adjacent (the spec says one kind per group).

Fix: decide the contract - iterate in field order (cost on the per-group state
machine to be measured) or record the deviation - and add the plain-before-dense
cat fixture with the expected order stated.

## VER-090 - The requirement-table check is convention-bound

`required` in `crates/harness/src/describe.rs` now refuses anything outside its convention
(exactly one root table, exact line shape, no duplicate ids, and any pbfhogg or library key
or nested include in an included file), and the authoritative composed check is
`brokkr library --all`. It still does not parse composed TOML; residue only if a
composed-configuration parse is ever wanted, which needs a brokkr configuration export
brokkr does not offer.

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
