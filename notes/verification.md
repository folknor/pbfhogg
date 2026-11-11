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
## VER-001 - test_host and Host::for_tests fix reported resources but not the executor

Reported by lib-integration, fault-injection, lib-core-unit, lib-commands-unit,
check-machinery.

`common::test_host` in `crates/lib/tests/common/mod.rs` and `Host::for_tests` in
`crates/lib/src/host.rs` report fixed resources but select `Executor::Ambient`,
and writer unit tests and the io_uring fault test choose it explicitly. Off a
Rayon worker that reaches the process-global pool, whose width depends on the real
host and Rayon configuration, and which concurrent tests contend for. Fixed
reported resources do not guarantee the concurrency a parked-worker scenario
needs. The `reference/testing.md` statement that tests see the same machine
everywhere is too broad today.

Fix: hand ordinary tests an owned executor with explicit width and reserve ambient
execution for tests of ambient behaviour. Enforcement: a fixture-host type and a
source rule allowing `Executor::Ambient` only in named tests.

## VER-002 - test_host mutates the process descriptor limit during fixture creation

Reported by lib-integration, fault-injection, cli-tests, check-machinery.

`common::test_host` raises `RLIMIT_NOFILE` once, discards the result, and returns a
host whose process policy is `NoProcessActions`. Whichever test calls first
decides when every sibling and later child changes. The `NoProcessActions` provider
still probes the real descriptor count, so allowances vary with sibling tests and
simultaneous descriptor-heavy tests can receive overlapping estimates, as the
contract in `crates/lib/src/host.rs` warns. A high hard limit masks the coupling;
a constrained host can refuse work or exhaust descriptors.

Fix: separate host-value construction from process initialization, make limit setup
an explicit harness step that reports its outcome, and coordinate descriptor-heavy
tests through a reserving budget or isolated execution. Enforcement: a rule that
host constructors and fixture factories make no process mutation, with real limit
scenarios in their own target.

## VER-003 - Unit test modules share one live invocation through statics

Reported by lib-core-unit, lib-commands-unit.

`crates/lib/src/output.rs` stores a publication token in a process-wide `OnceLock`,
and the `run_host` helpers in `commands/getparents/mod.rs` and
`commands/tags_filter/mod.rs` store an `Entered` host in one. Tests share the
publication log, execution lock, counters, diagnostic suppression, cancellation and
lifecycle. Independent scenarios serialize publication and keep each other's
records, and safety rests on a promise that the host is never cancelled, armed or
sealed. The comments justify sharing within a test, not across tests. Existing
fault-state rules do not catch these statics.

Fix: pass an owned per-test fixture holding the invocation and host. Enforcement:
ban static `Host`, `Entered` and `Invocation` values in test code, including inside
functions and `OnceLock`.

## VER-004 - entered_for_tests and entered_leaked leak every host

Reported by lib-core-unit, lib-commands-unit.

These helpers in `crates/lib/src/host.rs` use `Box::leak`, retaining each host,
invocation, instrumentation sink and fault-plan state until the binary exits. It
need not leak threads today, but it makes adding an owned executor dangerous and
prevents tests observing host destruction.

Fix: keep the host in the fixture and borrow an entered view, or scope both in a
closure helper. Enforcement: a lint against `Box::leak` in test infrastructure
outside documented lifetime tests.

## VER-008 - "Product builds never enable test-hooks" is a convention

Reported by fault-injection, cli-tests.

`crates/lib/Cargo.toml` and `crates/cli/Cargo.toml` expose ordinary additive
features that enable environment-controlled pauses, faults, file writes, input
replacement and scheduling controls in the production executable. Off-by-default
compilation is enforced; absence from product artifacts is not, and feature
unification can enable it.

Fix: a dedicated test executable or internal test build, or a product-build gate
inspecting the resolved feature graph. The gate checks release artifacts and does not
make arbitrary hook-enabled builds impossible. A blanket release-mode ban would
conflict with release-mode fault testing.

## VER-009 - The static inventory cannot see synthesized state

`scripts/check_source_invariants.py` now inventories every library static with counted,
audited exceptions and presence checks for protected files, tested in
`scripts/test_source_statics.py`. Unenforced: statics in synthesized macro output,
changes within audited initializers, and an architectural rule that `FaultPlan` is
constructed only through the invocation.

## VER-010 - The probe-lock and counter-emission guards are fixed-shape, not AST

The probe open now goes through `open_probe_lock` in `crates/lib/src/scratch.rs`,
tested for missing-file preservation, and counter emission lives in
`finish_invocation` in `crates/cli/src/lib.rs`, tested on success, refusal and
cancellation. The guards in `scripts/check_source_invariants.py` and
`scripts/rust_source_shapes.py` check a fixed lexical shape of each; they do not
prove arbitrary Rust control flow. Residue: a general AST check, if it pays.

## VER-012 - Source-policy checks are repeated and spread across separate parsers

Reported by lib-core-unit, cli-tests, check-machinery.

`scripts/check_source_invariants.py` scans library sources, CLI sources and library
sources again, rebuilding comment and literal masking each time, also on brokkr's
markdown-only shortcut. Measured at about 2.12 seconds in one execution at commit
`c95f9998` on bygg. The library text test in `crates/cli/src/library_text_rule.rs`
reads 203 files, about 6.53 MB, with its own lexer, cfg interpretation and module
exclusion, and runs in the `all`, `consumer` and `musl` sweeps. Markdown coverage
tests add another parser in the CLI unit build. `write/io_buffer_contract.rs` rescans
read and write sources in all five tier-1 build shapes (see VER-013).

Fix: one source-analysis entry point over one parsed corpus, run once per check, using
one maintained Rust parser, with the command schema exported once. Enforcement: wiring
into brokkr and no additional whole-tree scripts.

## VER-014 - Empty textlint globs and stale exclusions are only audit-enforced

Every rule in `brokkr.d/lints.toml` now has a `must_scan` sentinel, and the old coverage
script and its migration manifest are retired. brokkr still lets an empty `paths` glob or
a stale exclusion pass an ordinary check; only its audit flags them. Brokkr side: bz
brokkr#157.

## VER-018 - Passing skips are invisible and can satisfy the complete gate

Reported by lib-integration, cli-tests, check-machinery.

`skip` in `crates/lib/tests/common/skip.rs` only prints to stderr, which passing-test
capture hides, and brokkr sees a pass, so a mistaken `PlatformUnsupported` removes
backend coverage while staying green, as `reference/testing.md` warns. In
`crates/cli/tests/guard.rs`, `apply` silently returns when `supported()` is false, every
unsuccessful availability probe becomes `false`, and only the control test reports it,
as `skipped:`.

Fix: return a reasoned availability result and record executed, unsupported and failed
outcomes separately. Enforcement: structured capability accounting and a capability
gate requiring evidence for each promised backend and guard, while portable runs may
still skip.

## VER-019 - Two commands cannot take an injected input pin

36 of the 38 reopen cells run through the typed `Scenario::run` seam in
`crates/lib/tests/common/reopen_cell.rs` with injected pins. The public-external route of
add-locations-to-ways and geocode-build still use the older `available` helper, because
public `external_join` and `build_geocode_index` pin paths internally and accept no
caller pin; `crates/lib/tests/fault_apply_changes.rs` still uses it too. Residue: a
caller-pin entry point on each of the two commands, and migrating the apply-changes
fault test.

## VER-020 - Negative concurrency assertions pay sleeps and prove nothing about the wait

Reported by fault-injection, lib-core-unit, lib-integration, lib-commands-unit.

Sites wait real time and infer that another thread blocked. A descheduled thread passes
the assertion, or a valid ordering can fail it.

- `crates/lib/tests/fault_library_pools.rs`: 200 ms, three tests, at least 600 ms.
- `joined_pool.rs` and `read/pipeline.rs`: 200 ms each.
- `read/reader.rs` (three tests) and `read/pipeline.rs` queue close: at least 200 ms per
  build shape together. Sending "ready" before `acquire` does not establish `acquire`
  ran; the queue-close child can start after closure and pass without testing a wakeup;
  the admission-gate test can fail valid code.
- `crates/lib/tests/publication.rs` ordinal test 50 ms, and `PublishOrder` in
  `host_contract.rs` 500 ms plus two 20 ms: about 590 ms per sweep, about 2.95 s over
  five sweeps.
- `every_router_notification_waits_for_the_final_check_gate` in
  `altw/external/coord_payloads.rs`: 50 ms times four, at least 200 ms; the
  condition-variable wait also accepts a spurious wake as evidence.

Fix: add a per-instance observation at the actual wait or join transition and release
only after it, with timeouts only as failure bounds. Enforcement: a shared handshake or
contention fixture, a model test for the router protocol, and a lint against
timing-based negative assertions. A paused clock cannot establish thread ordering.

## VER-021 - Sleep-based scheduling does not establish the ordering tests claim

Reported by lib-core-unit, lib-commands-unit.

- `ordered_pool.rs`: twenty repetitions at 5 ms with no assertion the later failure
  finished first, a 200 ms slow head task, and a 20 ms reversal delay.
- `scan/classify.rs`: 24 blobs at 2 ms, positive stall assertions depending on
  scheduling.
- `write/parallel_gzip.rs`: a sink sleeping 40 ms per write.
- `threads.rs`: three tests with 50 ms sleeps.
- `encode_failing` in `apply_changes/stream_output.rs`: the earlier block waits on a
  flag, sleeps 20 ms, then fails. The comment admits this is a bias, but test
  descriptions say the later failure happens first. Four tests build a 16-thread pool so
  dependent blocks become separate jobs, so a Rayon splitter change can turn correct
  behaviour into a ten-second failure.

Fix: controlled completion gates or a controllable completion source at the
result-collection boundary, with a smaller real-executor test. Enforcement: a source
rule requiring an explicit exemption for test sleeps, and handshake tests. An injected
clock does not control OS scheduling.

## VER-023 - Timeout helpers and hang tests detach the work they time out

Reported by lib-core-unit, lib-integration.

`ordered_pool.rs` discards its spawned handle; helpers in `read/pipeline.rs` and
`read/reader.rs`, the tests in `crates/lib/tests/read_paths.rs` and
`renumber_within_deadline` in `renumber_external.rs` likewise detach. On timeout, workers
keep files, pipeline threads and executor work alive while later tests run, and the
fixture directory may be removed underneath them. A deadlocked Rust thread cannot be
safely reclaimed in-process.

Fix: run irrecoverable hang probes in a killable child process, and keep recoverable
rendezvous tests in-process with release-and-join guards or guaranteed cancellation.
Enforcement: one hang-test harness and a ban on discarded test thread handles outside
isolated targets.

## VER-026 - Timeout policy is duplicated and internal watchdogs lose to brokkr's

Reported by lib-core-unit, library-harness.

Helpers in `read/pipeline.rs` and `read/reader.rs` wait 30 s and promise a descriptive
deadlock failure, while `brokkr check` kills a test at 20 s. In the harness the concurrent
scenario gate in `scenarios/concurrent_cat.rs` waits up to 60 s, the executable runner
polls every 5 ms with a 15 s deadline, and FIFO readers poll every millisecond. A broken
rendezvous is killed before it can report.

Fix: a shared timeout policy below the outer watchdog or one process-owning authority,
event-driven readiness for FIFO and child completion, and one launch deadline budget.
Enforcement: configuration validation of deadline relationships and a failure-path test
with a short injected budget. Process and kernel deadlines still need real monotonic
time.

## VER-028 - Hook synchronization is spread across atomics and polling loops

Reported by fault-injection.

`crates/lib/src/test_hooks.rs` polls every millisecond with a fixed ten-second bound and
tests duplicate polling. Several scenarios rely on production seams and test code updating
related flags correctly. The fixed timeout is outside the tunables convention.

Fix: scenario objects owning transitions, notifications, release guards and observations,
with a real deadline only as a backstop, making invalid transition combinations
unrepresentable.

## VER-033 - Twelve library commands have no cancellation scenario

`crates/lib/tests/cancel.rs` keeps a registry of all 19 public library command modules,
compared mechanically against the module declarations: seven are exercised, twelve are
excluded explicitly, and checkpoint replays are capped at 64 per scenario. Scenarios for
the excluded twelve, and a comparison against the CLI command inventory, are not done.

## VER-036 - Chunk tests keep large fixtures despite size injection seams

Reported by fault-injection.

`crates/lib/tests/fault_diff_parallel.rs` uses 120,000 nodes to exceed the default chunk
and runs 60,000 nodes through 27 combinations including seven-byte chunks, at root tier
in the hooks sweep. Element-cut tests use hundreds of thousands of nodes.

Fix: small injected thresholds, one tiered production-default test, and an injected
planner policy. Enforcement: typed policy construction.

## VER-039 - The shared equivalence oracle discards meaningful metadata

Reported by lib-integration.

`meta_from_info` in `common/mod.rs` drops plain metadata when `version()` is absent, the
normalizers truncate milliseconds and collapse absent to zero or empty, and plain and
dense follow different presence rules, while `read/claims.rs` `MetaClaims` preserves
individual claims. Equivalence tests can miss lost unversioned metadata or subsecond
changes.

Fix: an independently decoded comparison model preserving presence and precision, with
narrower comparisons where commands intentionally change fields. Enforcement:
counterexamples differing by one field or one millisecond.

## VER-040 - The same oracle hides decode errors, duplicate tags and unknown member types

Reported by lib-integration.

The normalizers in `common/mod.rs` collect tags into `BTreeMap`, turn failed user and role
lookups into empty strings, and collapse unknown member types to `"unknown"`; the
coordinate reader swallows role errors.

Fix: preserve multiplicity and numeric unknown types, and fail on unexpected decode errors.
Enforcement: negative tests of the helpers and a scoped lint on error-to-empty conversions.

## VER-050 - Large apply-changes fixtures pay production thresholds to test scheduling

Reported by lib-integration.

`crates/lib/tests/apply_changes_create_runs.rs` generates 520,001 creates in one test and
528,001 plus 73 MiB of oversized tags in another, at root tier in five sweeps: over five
million creates and 365 MiB of oversized tag text per default check.

Fix: extract batch scheduling and ordered-error selection behind explicit limits and test
small, keeping format-limit coverage separate. Enforcement: observed batch events and a
selected tier for large scenarios.

## VER-054 - The canonical fixture writer is three implementations

Reported by lib-integration.

`write_blocks`, `write_indexed_pbf` and `write_test_pbf_non_indexed` in `common/mod.rs` repeat
traversal, metadata conversion and flushing, while parity tests claim inputs differ only in
indexdata.

Fix: generate logical blocks once and parameterize framing. Enforcement: verify paired fixtures
have identical payloads and only the intended envelope differences.

## VER-065 - Merge correctness fixtures scale with production batch tuning

Reported by lib-commands-unit.

Fixtures in `merge_changes/mod.rs` derive sizes from `MERGE_CHANGES_BATCH_BYTES`.
`rendered_simplify_writes_the_owned_bytes` does 24 simplify operations over 34,952 generated changes
at today's constant.

Fix: a batching configuration passed by production and tests, small boundary tests, and one focused
production-configuration test. Enforcement: the configuration type.

## VER-071 - The verbose sequence budget has no registered timing bench

The ignored timing probe in `commands/diff/seq.rs` is removed; its work-limit assertions
stay. Timing belongs in a registered bench: synthetic modified-way pairs with 30,000
refs (disjoint ranges, and every 60th ref changed), measuring `diff --verbose`, requiring
exit 1 and checking replacement versus itemized output, with fixture preparation outside
measurement. Not written: no existing command or bench helper produces those
pairs untimed, so the bench needs a fixture producer first.



## VER-078 - CLI children use the real machine

Reported by cli-tests.

`crates/cli/src/host.rs` builds `MachineProbe` resources and an ambient executor, while fixed
resources apply only to in-process fixture preparation. Tests can select different paths per host and
concurrent children size pools against the whole machine.

Fix: separate process setup from execution and hand command execution a resolved host and policy,
keeping subprocess tests for process integration. Enforcement: signatures and explicit child
configuration.

## VER-089 - Plans, execution and CLI counterparts describe the workload independently

Reported by library-harness.

Each scenario implements `decl()`, `plan()` and `run()` separately, and settings and their JSON are
duplicated in `scenarios/mod.rs`. The describe test checks names, not options, so a changed
implementation can keep its plan identity.

Fix: typed workload specifications generating the plan, the call and counterpart arguments. Types
enforce the representation; arbitrary handwritten behaviour cannot be proven equivalent.

## VER-090 - The requirement-table check is convention-bound

`required` in `crates/harness/src/describe.rs` now refuses anything outside its convention
(exactly one root table, exact line shape, no duplicate ids, and any pbfhogg or library key
or nested include in an included file), and the authoritative composed check is
`brokkr library --all`. It still does not parse composed TOML; residue only if a
composed-configuration parse is ever wanted, which needs a brokkr configuration export
brokkr does not offer.

## VER-098 - Release semantics and profiling configurations are not exercised

Reported by library-harness, check-machinery.

`[test] debug = true` applies to configured sweeps and none pins release. Excluding `hotpath` avoids
stdout contamination but leaves macro expansions, builder code and allocator configurations outside
checks; `check_hotpath_cpu.py` resolves metadata only. `run_from_env` in the harness activates a
profiling guard from `HOTPATH_OUTPUT_PATH`, and the executable helper always removes those variables.

Fix: release contract tests for build-mode behaviour, compile and lint-only `hotpath` and
`hotpath-alloc` shapes, and a small profiling lane with expectations for describe, inactive and active
modes. Enforcement: required shapes in the gate.

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

## VER-109 - The testing contract contradicts itself and duplicates inventories

Reported by check-machinery.

The stable allowlist in `reference/testing.md` says everything outside it needs CLI conversion, later
text permits `host` and `process`, `process_nofile` calls an internal entry point, and fault and
host-contract tests use more. It repeats feature and package lists and labels `full` a full gate.

Fix: state boundaries by test purpose, enforce through resolved imports or test-support dependencies, and
generate configuration tables. Claims such as every internal contract having a tier-1 test still need
human review.

## VER-110 - The testing record falsely says verify has no locations-on-ways path

Reported by bench-verify.

`reference/testing.md` says so, while brokkr's `verify_all.rs` runs that path when the keep-untagged base
resolves.

Fix: narrow it to what remains unavailable. Enforcement: generated capability inventories or a
machine-readable describe interface; the handcrafted-fixture rationale still needs review.

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
