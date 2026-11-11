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

## VER-005 - Fixture strings are static and force leaks in the shared process

Reported by lib-integration.

`TestNode`, `TestWay` and `TestRelation` in `crates/lib/tests/common/mod.rs` require
static tag strings. Three overflow generators in `crates/lib/tests/geocode_index.rs`
use `Box::leak`, leaking 196,608 names when all three run in one binary. The
explanation that the process exits soon no longer describes the shared binary.

Fix: borrowed lifetimes or owned or Cow strings. Enforcement: the type change, plus
a test-source lint on `Box::leak`.

## VER-006 - Test files can silently escape compilation

Reported by lib-integration, library-harness, cli-tests, check-machinery.

All three packages set `autotests = false`, so a file not declared in `all.rs` or
the manifest never compiles and no inventory can notice. `reference/testing.md` and
`crates/cli/tests/all.rs` acknowledge this. Every hunter that compared found no
orphan today; check-machinery traversed 60 library, 38 CLI and one harness file.

Fix: a pre-clippy, build-wired audit requiring each Rust file under a test directory
to belong to a declared target's module graph or a checked helper exemption, tested
in both directions, and verifying declared feature-gated modules land in an intended
sweep. This keeps the consolidated binaries.

## VER-007 - New harness scenarios need no executable test

Reported by library-harness.

All twelve registered scenarios have executable tests today, but adding one to
`scenarios::all()` does not require one, and the gate inventories compiled tests so
it cannot find the missing obligation.

Fix: drive basic scenario execution from structured case data. Enforcement: an exact
registry-to-executable-coverage check.

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

## VER-009 - The no-global-fault-state checks are narrower than their promise

Reported by fault-injection, check-machinery.

`gated_statics` in `scripts/check_source_invariants.py` reads only a static item's
preceding attribute block. check-machinery confirmed it returns no finding for a
static inside a `#[cfg(feature = "test-hooks")] mod`; macro-generated state is an
admitted gap. The companion textlint covers `test_hooks.rs` but not equivalent state
under another module name. No such state is claimed to exist today.

Fix: a Rust-aware check over the expanded item graph, or a ban on mutable statics
outside a small audited list regardless of feature spelling, with presence
assertions for protected files, an architectural rule restricting fault-state
construction to the invocation, and enclosing-module and macro cases in the checker
tests.

## VER-010 - The probe_lock guard checks tokens, not the open operation

Reported by lib-core-unit, check-machinery.

`scripts/check_source_invariants.py` extracts `probe_lock` by string splits, requires
`.write(true)` and bans `.create(`. It accepts `.create_new(true)`, a later
`.write(false)`, a string or an unrelated builder, delegation to a helper escapes it,
and extraction depends on brace layout. `probe_lock` in `crates/lib/src/scratch.rs`
is correct today; the unit test `the_probe_opens_the_lock_for_writing` delegates the
claim to this guard, and the guard does not establish the ADR-0018 claim that
scanning never creates a lock file.

Fix: put probe-open options in one constructor whose interface exposes no creation
option, test that a missing `LOCK` stays missing and contents are preserved.
Enforcement: an AST check restricting open flags, with negative fixtures per
forbidden option.

## VER-011 - The counter-hook check accepts strings and unreachable code

Reported by check-machinery, reproduced in memory by that hunter.

`scripts/check_source_invariants.py` strips only `//` suffixes and whitespace and
searches CLI source for two strings. A string literal holding the
`emit_writer_counters` call gave a false pass; a block comment, unused function or
unreachable branch can too. This contradicts the script's claim that comments and
literals cannot decide the outcome.

Fix: move final counter emission into an invocation-finalization operation that owns
the lifecycle, tested on successful, refused and cancelled exits. An AST check can
enforce placement; substring presence cannot.

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

## VER-013 - The I/O-size source guard is repeated and spelling-bound

Reported by lib-core-unit.

`crates/lib/src/write/io_buffer_contract.rs` scans the read and write trees at test
runtime. It catches only spellings equivalent to `256*1024` and `262144`, so `1 << 18`,
`128 * 2048` or hexadecimal bypass it; comments, strings and unrelated digits can fail
it; an emptied directory passes.

Fix: move it to the once-per-check phase with expected-file assertions and parsed
expressions or a structural transfer-buffer abstraction, with runtime tests of
alignment and boundaries.

## VER-014 - Most textlint rules can lose their whole corpus silently

Reported by check-machinery.

Brokkr lets a `paths` glob matching nothing pass. Some rules use `must_scan`, many do
not, including those for environment access, host probes, process mutation and
stdout. `scripts/textlint_coverage.py` detects dead selectors but no check
or gate invokes it, its manifest under `notes/` is a migration artifact, and it
duplicates brokkr's selector semantics.

Fix and enforcement: require nonempty selectors in brokkr, including stale exclusions
and sentinel membership. A pinned list of admitted files is unnecessary.
Brokkr side filed as bz brokkr#157.

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

## VER-019 - The input replacement matrix silently loses its procfs arm

Reported by fault-injection.

Tests such as those in `crates/lib/tests/fault_time_filter_input.rs` request forced
path reopening and automatic reopening, then adapt to `reopens_through_procfs()`.
Without procfs reopening both cells exercise path reopening, unannounced. On non-Unix
systems, `else if cfg!(unix)` branches leave intermediate-replacement outcomes
unchecked.

Fix: announce the skip, do not rerun the identical cell, and separate reopen strategy
behind an injected opener. Enforcement: a scenario runner requiring every cell to
return a checked outcome.

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

## VER-022 - Counter tests wait for values they could supply

Reported by lib-core-unit.

`write/metrics.rs` sleeps 2 ms before `record_sync_all`, which already accepts elapsed
nanoseconds. `counters.rs` couples stopwatch arithmetic to real time, and its blocked
operation tests use 10 ms release delays retried up to twenty times: normally 30 ms, up
to 600 ms.

Fix: pass fixed values and assert exact totals, separate elapsed conversion from clock
acquisition, and observe slow-path entry. Enforcement: exact-value tests and a ban on
sleeps in counter unit tests.

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

## VER-024 - The stalled-transform test has an unbounded failure-path leak

Reported by lib-core-unit.

A test in `read/pipeline.rs` spawns an orchestration thread without keeping its handle,
and its transform spins on `release`, set only after observing `stalled`. If the first
observation times out, a worker arriving later can spin forever.

Fix: an RAII guard that releases and joins on every exit path, following the nearby
`ReleaseTeardowns` pattern. Enforcement: a common parked-worker fixture and the
discarded-handle lint.

## VER-025 - The parked-reader fixture can deadlock while unwinding

Reported by lib-integration.

`ParkingRead` in `crates/lib/tests/capabilities.rs` blocks on an unbounded receive, and
the block iterator is created after the release sender. A failed assertion drops the
iterator first, its pipeline join waits for the reader, and the sender stays alive
further up, so a useful failure becomes a watchdog kill.

Fix: bundle iterator and release in a guard that releases before joining. Enforcement: a
test that unwinds while parked.

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

## VER-027 - FIFO capture polls, is duplicated, and leaks on unwind

Reported by lib-integration, library-harness.

`Capture` in `crates/lib/tests/seal.rs` and the capture in `host_contract.rs` poll with
1 ms sleeps and duplicate setup. `Capture` joins only through `finish`. `Fifo` in
`crates/harness/tests/exe.rs` has no `Drop`, so a panic detaches a reader that polls
forever with its descriptor open in the merged binary, and reader errors are swallowed.
Both library sites need UTF-8 paths while `make_fifo` accepts native bytes.

Fix: one owning fixture with event-driven reads, explicit shutdown and joined cleanup, and
equivalent ownership of child processes. Enforcement: deliberate-unwind and non-UTF-8
tests, and a ban on local FIFO-drain implementations.

## VER-028 - Hook synchronization is spread across atomics and polling loops

Reported by fault-injection.

`crates/lib/src/test_hooks.rs` polls every millisecond with a fixed ten-second bound and
tests duplicate polling. Several scenarios rely on production seams and test code updating
related flags correctly. The fixed timeout is outside the tunables convention.

Fix: scenario objects owning transitions, notifications, release guards and observations,
with a real deadline only as a backstop, making invalid transition combinations
unrepresentable.

## VER-029 - Installing a hook callback twice silently keeps the old one

Reported by fault-injection.

`crates/lib/src/host.rs` and two sites in `test_hooks.rs` discard `OnceLock::set` failures,
so a fixture and caller can both configure a hook and only the first applies.

Fix: return a must-use error or panic on duplicates, or install while constructing the
scenario. Enforcement: types and signatures.

## VER-030 - The io_uring panic test can pass on an unrelated failure

Reported by fault-injection.

`crates/lib/tests/fault_uring_writer.rs` accepts any error and any unwind. Neither proves
dispatch 3 reached the injected panic, and an unwind is accepted despite the described
error through `flush`.

Fix: observe the dispatch count, require the injected payload, and reject unwinds if an
error is the contract. Enforcement: a shared fault-result assertion helper and a
regression test supplying an unrelated error.

## VER-031 - The geocode pass 3 fault test accepts what its contract rejects

Reported by fault-injection.

`crates/lib/tests/fault_geocode_pass3.rs` says a worker panic must surface as `Err`, but
accepts an unwind carrying the injected message, so removing panic-to-error handling can
stay green.

Fix: require `Ok(Err(e))`, verify provenance, then check leftovers, or change the claim if
unwinding is intended. Enforcement: the result shape in the test.

## VER-032 - The cancellation sweep accepts runs that never cancel

Reported by fault-injection.

`crates/lib/tests/cancel.rs` picks checkpoint ordinals from a counting run and then ignores
successful runs at them, requiring only one cancellation per command. Scheduling or
checkpoint changes can remove coverage silently.

Fix: record whether each callback fired, distinguish unreachable ordinals from cancellation
too late to affect a decided publication. Enforcement: a structured scenario result.

## VER-033 - "Every checkpoint" and "every command" overstate cancellation coverage

Reported by fault-injection.

The sweep in `cancel.rs` samples when the count exceeds 64 despite its comment, and its
registry has ten `commands` entries, omitting getid, getparents, repack, renumber,
tags-filter, tags-count and time-filter.

Fix: rename the claims now. For complete coverage, a command-scenario registry with
explicit modes and exclusions. Enforcement: mechanical comparison against the command
inventory.

## VER-034 - The census test can pass without observing anything

Reported by fault-injection.

The test in `cancel.rs` starts its maximum at zero, checks it stays zero, and does not
assert the callback ran.

Fix: require a positive observation count and narrow the claim to observed checkpoints.
Enforcing the stronger invariant belongs at issuance, through an explicit
internal-versus-issued construction path or an observer.

## VER-035 - The publication observer docs are false about locking

Reported by fault-injection.

`test_hooks.rs` promises "no lock held", but publication runs under the invocation's
execution lock per `output.rs` and `reference/testing.md`. An observer waiting on another
publication under that invocation can form a cycle.

Fix: correct the contract and state allowed blocking dependencies. Same-invocation
re-entry can be tested; arbitrary cross-thread cycles cannot be fully enforced.

## VER-036 - Chunk tests keep large fixtures despite size injection seams

Reported by fault-injection.

`crates/lib/tests/fault_diff_parallel.rs` uses 120,000 nodes to exceed the default chunk
and runs 60,000 nodes through 27 combinations including seven-byte chunks, at root tier
in the hooks sweep. Element-cut tests use hundreds of thousands of nodes.

Fix: small injected thresholds, one tiered production-default test, and an injected
planner policy. Enforcement: typed policy construction.

## VER-038 - assert_sorted_file cannot see type interleaving and misorders negative IDs

Reported by lib-integration.

`assert_sorted_file` in `crates/lib/tests/common/mod.rs` reads through a helper that splits
kinds into vectors, so way 1 before node 1 passes. Raw `i64 >=` rejects canonical order
such as `-1, -2`, which `crates/lib/tests/check.rs` recognizes. This weakens apply-changes
and renumber assertions today.

Fix: check `(kind, canonical ID key)` while traversing. Enforcement: interleaved, inverted
and mixed-sign control fixtures.

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

## VER-041 - An empty output satisfies the normalized-content assertions

Reported by lib-integration.

`read_normalized` in `common/mod.rs` ignores headers and returns empty sections for empty
input, so `empty_pbf_survives_sort` in `edge_cases.rs` accepts a zero-byte output.

Fix: successful-output readers require a valid header and complete framing. Enforcement:
empty, missing-header and malformed-tail controls.

## VER-042 - One metadata assertion is absent behind an obsolete explanation

Reported by lib-integration.

`test_meta_roundtrips_through_writer` in `crates/lib/tests/fixture_helpers.rs` ends with
`let _ = n2.meta.as_ref()`, which cannot fail, and its fabricated-zero explanation
contradicts `roundtrip_mixed_metadata`.

Fix: assert `None` on the decoded node. A narrow lint can reject discarded expressions in
assertion helpers but cannot judge adequacy in general.

## VER-043 - The all-blobs index guard passes with no readable blobs

Reported by lib-integration.

`assert_every_data_blob_indexed` checks the prefix `locate_blobs` in `common/adversarial.rs`
managed to read, and that stops at malformed framing. Empty and malformed inputs pass.

Fix: separate strict and prefix-tolerant framing APIs, the strict one returning extent and
frame kinds. Enforcement: strict-guard malformed tests and a data-presence requirement.

## VER-044 - Fixture descriptions are false and smoke tests do not hold them

Reported by lib-integration.

`write_bad_indexdata_pbf` promises 30 zero bytes but `resize(30, 0)` truncates a 42-byte
index, keeping version and prefix. The multiblock example says seven data blobs where its
smoke test expects six. The base and changed contract test checks node and way IDs only,
despite claiming exactly the documented edits.

Fix: assert exact bytes and compare complete contents against explicit edits. Executable
examples can enforce counts; prose cannot be guaranteed.

## VER-045 - Plain-node geocode coverage still requires installed osmium

`legal_non_dense_nodes_build_a_queryable_index` remains an ignored external test
and skips when osmium is absent. Its launches now use the shared bounded oracle
runner, deliberately resolved executable, private cwd/TMPDIR and version report.
The remaining coverage gap is independent of child ownership: an ordinary
synthetic plain-node fixture should exercise this repository's handling without
an external tool. Extend `common/groups.rs` for that coverage; parity work can
stay external. This coverage change is outside the filesystem chain.

## VER-047 - process_nofile assumes more about Linux than its subject needs

Reported by lib-integration.

`crates/lib/tests/process_nofile.rs` skips everything below a hard limit of 1024, requires
procfs counting, and assumes every descriptor below its highest held one is occupied, so an
inherited hole invalidates it. A sandbox without procfs fails where production represents an
unavailable count.

Fix: separate counting from limit and budget scenarios, compare controlled deltas, and handle
unavailable procfs. Enforcement: isolated scenarios and injected counting results.

## VER-048 - Proptest configuration inherits the environment

Reported by lib-integration. Narrowed by the test-filesystem fixture slice:
failure persistence is now explicitly disabled and failing cases are promoted by
hand to named tests, so no regression-path or checkout-write residue remains.

`crates/lib/tests/proptests.rs` still uses `ProptestConfig::default()`, and the
macros reapply `PROPTEST_*` after `cases: 64`, so `PROPTEST_CASES=0` removes
coverage while passing. Seed policy is still inherited too.

Fix: deliberate environment and seed policy, with tests of resolved settings.
This is outside the test-filesystem chain's persistence-only change.

## VER-049 - Property tests spend heavily on a narrower input space than described

Reported by lib-integration.

Seven properties are 448 cases per sweep, not the documented `64 * 4`. Five create disk
fixtures: 320 directories per sweep, 1,600 over five sweeps. Tags are empty, relation members
are all ways with one role, metadata is absent. The truncation property does not assert
refusal, and the arbitrary-reader one does not prove termination.

Fix: memory-backed codec fixtures, independent semantic generators, and separate no-panic,
termination and truncation assertions. Enforcement: strategy control cases and shared runner
tests.

## VER-050 - Large apply-changes fixtures pay production thresholds to test scheduling

Reported by lib-integration.

`crates/lib/tests/apply_changes_create_runs.rs` generates 520,001 creates in one test and
528,001 plus 73 MiB of oversized tags in another, at root tier in five sweeps: over five
million creates and 365 MiB of oversized tag text per default check.

Fix: extract batch scheduling and ordered-error selection behind explicit limits and test
small, keeping format-limit coverage separate. Enforcement: observed batch events and a
selected tier for large scenarios.

## VER-051 - The batch fixture guard checks a second handwritten constant

Reported by lib-integration, lib-commands-unit.

`BOUNDARY_RUN_FULL_BLOCKS = 65` in `apply_changes_create_runs.rs` and
`BOUNDARY_FIXTURE_FULL_BLOCKS = 65` in `commands/apply_changes/stream_output.rs` are
connected only by a comment.

Fix: assert the actual batch transition from the integration invocation, which also catches
changed dispatch, or share a definition that constructs the fixture.

## VER-052 - The io_uring size sweep emits unintended blobs and misses its boundaries

Reported by lib-integration.

`roundtrip_uring_small_size_sweep` in `roundtrip.rs` calls `bb.take()` after every node,
giving 26,611 one-node blobs over seven cases although a comment says the 16,000-node case
makes two. Node counts with compression do not establish the stated 4 KiB and 256 KiB
positions.

Fix: drive exact byte lengths through the writer layer, assert logical length and submission
behaviour, keep a small roundtrip, and derive cases from the alignment contract.

## VER-053 - Whole-file feature gates remove unrelated coverage

Reported by lib-integration.

`crates/lib/tests/all.rs` gates all of `geocode_index.rs` on `commands`, so the
`geocode-reader` sweep cannot run reader tests, and gates `tags_count.rs` though `tags_count`
is in the always-compiled surface per ADR-0011.

Fix: separate builder setup from reader tests with independently encoded fixtures, and remove
unneeded gates. Enforcement: expected test presence per build shape.

## VER-054 - The canonical fixture writer is three implementations

Reported by lib-integration.

`write_blocks`, `write_indexed_pbf` and `write_test_pbf_non_indexed` in `common/mod.rs` repeat
traversal, metadata conversion and flushing, while parity tests claim inputs differ only in
indexdata.

Fix: generate logical blocks once and parameterize framing. Enforcement: verify paired fixtures
have identical payloads and only the intended envelope differences.

## VER-056 - Reader constructors bring machine probes into tests

Reported by lib-core-unit.

Default worker selection in `read/reader.rs` and `read/pipeline.rs` falls back to `MachineProbe`
when resources were not attached, and inline tests start parallel iteration without fixing the
decoder count.

Fix: a resource-injection builder for public readers, keeping convenient defaults. Enforcement: a
structural fixture API and a lint on unconfigured parallel test readers.

## VER-059 - The cgroup parent-limit test never exercises parent discovery

Reported by lib-core-unit.

`cgroup_memory_budget_caps_host_and_parent_headroom` in `host.rs` calls `cgroup_memory_available`
twice for its parent case, and `machine_memory_available` reads kernel files directly, so ancestor
walk, mount mapping, mountinfo decoding and membership selection have no seam.

Fix: a narrow kernel-file reader and complete synthetic hierarchies, including unlimited leaves
with limited parents and delegated mounts.

## VER-060 - Scratch identity generation has no deterministic seam

Reported by lib-core-unit.

`scratch.rs` directly acquires randomness and the PID and `random_u128` calls `getrandom`, so
interruption, entropy failure and repeated-name collision cannot be exercised.

Fix: an internal constructor accepting identity generation with scripted generators. Enforcement:
restrict direct entropy and PID acquisition to the adapter with a lint.

## VER-061 - Output unit tests bypass the production binding protocol

Reported by lib-core-unit.

Shorthands in `output.rs` open writing sessions on unbound outputs and bind only at publication,
and `OutputDir::writer` ignores its invocation argument. They are `cfg(test)` and not
production-reachable, but they preserve an alternate protocol.

Fix: rewrite around owned bound-output fixtures and remove the methods, so the compiler enforces
sequencing.

## VER-062 - The pipeline-death test replaces the shared panic hook

Reported by check-machinery.

A test in `read/reader.rs` replaces the process panic hook while concurrent unit tests run, and
restoration is manual, so an unexpected unwind leaves it installed.

Fix: remove the mutation or inject the failure, or use a dedicated process. Enforcement: a lint
banning hook replacement in shared test targets.

## VER-063 - The merge executor test accepts fully serial execution

Reported by lib-commands-unit.

`encode_jobs_stay_on_the_executor_within_their_lanes` in `merge_changes/stream.rs` sleeps 2 ms per
job, accepts any peak from one through `lanes`, and uses 262,144 XML nodes under three pool
configurations.

Fix: small synthetic batches and a handshake establishing two simultaneous jobs, with sequence
order checked if the comment's claim belongs there. Enforcement: the test harness, with batch size
supplied to the parser mechanism.

## VER-064 - Location-store tests create 3,000 threads for probabilistic coverage

Reported by lib-commands-unit.

Concurrent join tests in `commands/check/locations/store.rs` do 200 repetitions per case at three
threads each, 1,800 and 1,200, as tier-1 in every applicable sweep.

Fix: model the atomic state machine, keep a bounded smoke test, and put extended stress in a stress
lane. Repetition cannot enforce interleaving coverage.

## VER-065 - Merge correctness fixtures scale with production batch tuning

Reported by lib-commands-unit.

Fixtures in `merge_changes/mod.rs` derive sizes from `MERGE_CHANGES_BATCH_BYTES`.
`rendered_simplify_writes_the_owned_bytes` does 24 simplify operations over 34,952 generated changes
at today's constant.

Fix: a batching configuration passed by production and tests, small boundary tests, and one focused
production-configuration test. Enforcement: the configuration type.

## VER-066 - A complete retired merge implementation is compiled as a test oracle

Reported by lib-commands-unit.

`merge_changes/mod.rs` keeps a test-only owned representation, parser sink, stream construction and
writer, run through `owned_simplify`. The hunter calls this meaningful differential coverage, not a
tautology, but shared parser and version-floor components limit its independence.

Fix: a small semantic reference model and independent rendered-output checks, with byte comparisons
only where byte identity is the contract. Confinement to a test module is enforceable; independence
needs review.

## VER-067 - The geocode defaults test overwrites the defaults it validates

Reported by lib-commands-unit.

`rejects_invalid_levels_and_radii` in `geocode_index/builder/mod.rs` overwrites four fields before
asserting the defaults validate, and a later comment about the default radius describes a changed
value.

Fix: assert on an untouched default and build each invalid case independently with its intended
refusal.

## VER-068 - The geocode reader absence test never calls the reader methods

Reported by lib-commands-unit.

`defensive_fixed_record_readers_expose_absence_in_their_types` in `geocode_index/reader.rs` assigns
four methods to function pointers and tests `Table::get` on empty tables. Replacing a method body
with a panic stays green.

Fix: invoke each method on a minimal reader with absent and present records and move table behaviour
beside `Table`.

## VER-069 - Sparse allocation failure coverage stops before the syscall

Reported by lib-commands-unit.

Tests in `commands/altw/sparse.rs` request `u64::MAX`, which the free-space precheck refuses before
`posix_fallocate`, so `failed_preallocation_leaves_no_file_in_scratch` does not exercise allocation
failure. The success test requires a filesystem supporting `posix_fallocate`.

Fix: separate policy from allocation, inject success and failure, and keep a named platform adapter
test. Enforcement: an allocation interface and fault tests.

## VER-070 - The sparse unlink assertion is keyed to the production filename

Reported by lib-commands-unit.

`successful_allocation_unlinks_the_store` checks only that no entry is named `sparse-index`.

Fix: compare complete directory contents before and after and verify descriptor contents.

## VER-071 - A manual release timing probe runs in the full correctness profile

Reported by lib-commands-unit.

`verbose_sequence_budget_timing` in `commands/diff/seq.rs` is ignored as manual release-mode timing,
but the `full` profile includes ignored tests under `[test] debug = true`, running 22 timed calls in
a configuration unlike the annotation. It has correctness assertions.

Fix: split work-limit assertions from timing and move timing to a registered bench. Enforcement: a
source check against timing probes in correctness targets.

## VER-073 - The truncation sweep's "every command" claim is false

Reported by cli-tests.

The opening comment says every command, and the body hardcodes six.

Fix: an explicit representative read-path matrix with named obligations, or enumeration with
exemptions. Enforcement: a build-wired inventory comparison.

## VER-074 - The truncation matrix is one watchdog unit

Reported by cli-tests.

Up to `6 * (8B + 12)` children for `B` blobs, each with an eight-second timeout, under one 20 s
watchdog, repeated across CLI sweeps.

Fix: independently named cases or bounded shards, byte-boundary coverage at the library reader, and
CLI cases for routing and propagation. Enforcement: mechanical cell inventory and maximum shard size.

## VER-075 - CliInvoker::run adds a polling delay to every subprocess

Reported by cli-tests.

`crates/cli/tests/invoker.rs` polls every 20 ms, so each child can wait up to one interval, which
over hundreds of quick children is seconds per test.

Fix: blocking completion with an independent timeout. Enforcement: an invoker test with a controlled
child, without a host-speed assertion.



## VER-078 - CLI children use the real machine

Reported by cli-tests.

`crates/cli/src/host.rs` builds `MachineProbe` resources and an ambient executor, while fixed
resources apply only to in-process fixture preparation. Tests can select different paths per host and
concurrent children size pools against the whole machine.

Fix: separate process setup from execution and hand command execution a resolved host and policy,
keeping subprocess tests for process integration. Enforcement: signatures and explicit child
configuration.

## VER-079 - Cancellation and replacement hook setup fails open

Reported by cli-tests.

In `crates/cli/src/signals.rs`, malformed pause specifications return silently and ready, ack and
count file writes are discarded. Input replacement returns on invalid configuration and its report
writer discards errors. Publication and merge-spill hooks fail explicitly.

Fix: parse hook configuration into validated types before dispatch and give the invoker typed hook
methods. Enforcement: negative configuration tests.

## VER-081 - The timestamp test does not prove the current time

Reported by cli-tests.

`cli_derive_changes.rs` accepts any timestamp beginning `20`, and production reads `SystemTime::now`
in `crates/cli/src/lib.rs` with no injection point.

Fix: resolve time at the process boundary and test exact per-element values with a fixed instant,
keeping a small clock-wiring test. Enforcement: the signature and exact-value tests.

## VER-082 - Command-table checks discover capabilities by parsing help

Reported by cli-tests.

`cli_flag_contract.rs` and `cli_output_contract.rs` depend on spellings such as `-j, --jobs`, a line
starting `--force` or a `--tmp-dir` substring. The jobs check requires only five matches and one
direction, children start per row, and the output-contract claim about binary help is stale.

Fix: inspect built Clap arguments, compare both directions, and keep rendered-help tests for
presentation. Enforcement: a shared command-tree accessor.

## VER-083 - The library text rule stops recognizing stale advice after a rename

Reported by cli-tests.

`names_subcommand` in `crates/cli/src/library_text_rule.rs` matches `pbfhogg <word>` only for current
commands, which is exactly the stale text the module exists to prevent.

Fix: detect invocation-shaped advice independent of membership, or separate diagnostic prose from
frontend advice. Enforcement: a rename and removal regression fixture.

## VER-084 - The library text scanner's exclusions can hide production strings

Reported by cli-tests.

`library_literals` excludes paths named by test-only module declarations, which is not reachability:
a file included from both test and production through `#[path]` is excluded. No current violation
was established.

Fix: parsed reachability, or a conservative lexical policy with exemptions. Enforcement: shared-file
inclusion and path-resolution tests.

## VER-085 - Generated reference data is maintained by hand

Reported by cli-tests.

`crates/cli/src/io_reach.rs` compares a Markdown matrix with `matrix()`, `lib.rs` parses the
short-flag table back, and the documentation checker reconstructs inventories from Markdown.

Fix: generate the command, option, vocabulary and I/O sections, keeping authored prose. Enforcement:
a check mode, with writers comparing bytes before replacing a file.

## VER-086 - The bad-request executable test can pass when validation breaks

Reported by library-harness.

`bad_requests_are_refused_quietly` in `crates/harness/tests/exe.rs` gives each request a nonexistent
marker FIFO, so failing at `MarkerFifo::open` yields every expected result.

Fix: otherwise working launches with a live FIFO, a checked refusal reason and a valid control.
Enforcement: mutation tests disabling each validation branch.

## VER-087 - scenario_passes_on accepts missing coverage

Reported by library-harness.

The helper in `exe.rs` checks only that collected results say `pass`, and empty arrays satisfy
`.all(...)`. Brokkr's consumer validates this, outside repository checks.

Fix: compare exact call and assertion inventories through a shared protocol validator. Enforcement:
missing, duplicate, misplaced and undeclared negative fixtures.
Brokkr side (reusable validator) filed as bz brokkr#159; it also serves the next entry's runtime inventory check.

## VER-088 - The harness runner can succeed without completing its declaration

Reported by library-harness.

`run_launch` and `finish_call` in `crates/harness/src/runner.rs` never establish that every declared
call and assertion ran exactly once. Brokkr catches this later, but the exit status misleads.

Fix: execution consumes declared handles and finalization refuses unused or repeated ones, or a
runtime inventory check. Enforcement: tests that omit and duplicate.

## VER-089 - Plans, execution and CLI counterparts describe the workload independently

Reported by library-harness.

Each scenario implements `decl()`, `plan()` and `run()` separately, and settings and their JSON are
duplicated in `scenarios/mod.rs`. The describe test checks names, not options, so a changed
implementation can keep its plan identity.

Fix: typed workload specifications generating the plan, the call and counterpart arguments. Types
enforce the representation; arbitrary handwritten behaviour cannot be proven equivalent.

## VER-090 - The requirement-table check reads a formatting convention

Reported by library-harness.

`required` in `crates/harness/src/describe.rs` scans root `brokkr.toml` for a table header and lines
beginning with a double quote, and does not parse TOML or compose includes, so partial omission
passes.

Fix: parse the effective configuration or use one structured file. Enforcement: equivalent-format
and included-file fixtures.

## VER-091 - Descriptor scenarios depend on the inherited process

Reported by library-harness.

`Nofile::Soft` refuses below a hard limit of 1024, while executable tests expect a pass, and
`nofile-altw-external` expects exactly three workers from what remains after ambient opens and global
pool initialization. Most launches use machine parallelism. This makes the "No test depends on the
host" claim in `reference/testing.md` false for this scope.

Fix: inject exact allowances and keep a small child contract test for real limits, detecting
unsupported hard limits. Enforcement: fixed resources, pool sizes and restricted-launch tests.

## VER-092 - Descriptor exhaustion does nearly a thousand real opens

Reported by library-harness.

`fill_descriptors` in `scenarios/nofile.rs` opens repeatedly until eight remain, in three sweeps, and
silently opens nothing with fewer than eight free. `/proc/self/fd` counting assumes procfs.

Fix: inject the allowance and test counting in a child, asserting the established free count.
Enforcement: a fixture type carrying the verified allowance.

## VER-093 - Harness assertions allocate output-sized memory

Reported by library-harness.

`outputs_equal` in `scenarios/repeat_cat.rs` reads both outputs, about `2S` heap, which can OOM at
planet scale and inflates process measurements. `SeededReplace::run` reads each destination into a
`Vec`, where brokkr's `seed::holds` is bounded.

Fix: compare lengths then bounded chunks, and a bounded seed-state helper also used in
`check_bindings`. Enforcement: the helpers, a source rule against whole-file reads in scale-facing
assertions, and edge-case tests.

## VER-095 - Harness fixtures build unused inputs and substitute paths into JSON

Reported by library-harness.

`scenario_passes_on` always writes indexed and raw fixtures, and `RunDir::new` substitutes paths into
JSON text without escaping.

Fix: build only requested inputs and serialize with `serde_json`. Enforcement: JSON-sensitive paths
and constructors requiring an input recipe.

## VER-096 - The diagnostic-closure assertion samples and does not prove quiescence

Reported by library-harness.

`late_report` in `runner.rs` checks counters once before `scenario.json`, while the assertion's wording
says no call diagnostic arrived after its document.

Fix: keep delivery inside a joined execution scope and describe the residual limit. Enforcement:
owned-task completion and a delayed-emission test. Complete future silence cannot be proven by a
finite sample.

## VER-097 - Harness diagnostic collection is unbounded

Reported by library-harness.

`Collector::event` in `crates/harness/src/diag.rs` formats whole messages before truncating and keeps
every event, and brokkr refuses artefacts over 16 MiB only after the cost is paid.

Fix: bounded formatting and a collection budget with an overflow verdict. Enforcement: oversized
message and many-event tests.

## VER-098 - Release semantics and profiling configurations are not exercised

Reported by library-harness, check-machinery.

`[test] debug = true` applies to configured sweeps and none pins release. Excluding `hotpath` avoids
stdout contamination but leaves macro expansions, builder code and allocator configurations outside
checks; `check_hotpath_cpu.py` resolves metadata only. `run_from_env` in the harness activates a
profiling guard from `HOTPATH_OUTPUT_PATH`, and the executable helper always removes those variables.

Fix: release contract tests for build-mode behaviour, compile and lint-only `hotpath` and
`hotpath-alloc` shapes, and a small profiling lane with expectations for describe, inactive and active
modes. Enforcement: required shapes in the gate.

## VER-099 - Brokkr builds a release CLI even with no CLI counterpart

Reported by library-harness.

`LibraryHarness::build` in brokkr builds a release CLI on every real run, so `page-cache-keep` and the
refusal scenarios build what they never use, documented and deliberate in brokkr's library-internals
reference. Cost not measured.

Fix: resolve build needs from scenario metadata and build matching configurations together. Enforcement:
build-planning tests.
Brokkr side filed as bz brokkr#158.

## VER-100 - The pre-merge runbook does not run the complete gate

Reported by check-machinery.

`reference/testing.md` calls `--profile full` the before-merge tier, but `full` selects only the `all`
sweep, without other feature shapes, doctests, execution accounting or gate-only member checks, and
legacy profiles exit with `check passed`.

Fix and enforcement: make the merge command `brokkr check --gate`, enforced in CI or a merge
prerequisite, and keep focused profiles as explicitly partial.

## VER-101 - The serial profile does not guarantee serial execution

Reported by check-machinery.

`brokkr.d/checks.toml` sets `test_threads = 1` but references `all`, which has `parallel = {}`, so
binaries run concurrently.

Fix: remove the placeholder or give it the correct shape. Enforcement: a serial-claiming profile may
reference no parallel sweep.
The pbfhogg config fix is ours; the brokkr-side refusal is filed as bz brokkr#166.

## VER-102 - Rustdoc link validation is not enabled

Reported by check-machinery.

The composed configuration has no `[rustdoc]` table, so that phase is inert, and
`scripts/check_doc_links.py` does not resolve intra-doc links. Whether links are broken today was not
run.

Fix and enforcement: enable the phase and require it in the gate.

## VER-103 - The Markdown checker has a fence-parsing defect

Reported by check-machinery, reproduced in memory by that hunter.

Anchor and paragraph extraction in `scripts/check_doc_links.py` toggle a Boolean on any fence line. A
four-backtick fence holding a three-backtick line caused both a false examination and a missed real
link.

Fix: tokens from a parser with compatible fence semantics. Enforcement: regression cases for delimiter
lengths, types and embedded examples.

## VER-104 - The document checker reimplements renderer behaviour unverified

Reported by check-machinery.

It implements GitHub and VitePress slugging, inline rendering, link recognition and part of TypeScript
sidebar parsing with no self-test corpus, and recognizes ATX headings only, so valid Setext links can
be rejected.

Fix: test against committed examples and actual renderer output and use renderer identifiers where
possible, with an explicit tested GitHub compatibility contract.

## VER-105 - The migration inventory discards identity

Reported by check-machinery, cli-tests.

`scripts/test_inventory.py` writes package, target kind and name, then ignores them in comparison,
ignores build shape, and skips non-dictionary inventories. `Counter` detects reduced multiplicity, but a
move to another package or target preserves the count.

Fix: compare complete identities with explicit, stale-checked mappings and refuse a claim when an
inventory is missing.

## VER-106 - The declared minimum Rust version is unchecked

Reported by check-machinery.

Root `Cargo.toml` declares `rust-version = "1.99"` and says only nightly is installed and no MSRV check
is planned. This is not evidence 1.99 fails.

Fix: a library and installable-CLI build under the declared version with the toolchain derived from the
manifest, or remove the claim.

## VER-107 - The musl sweep assumes an x86_64 host

Reported by check-machinery.

`brokkr.d/checks.toml` always targets `x86_64-unknown-linux-musl`. On aarch64 it is a cross-build with
foreign executables.

Fix: separate compile-only from executable coverage and resolve a runnable target from an explicit host
capability, or check the requirement before building. Enforcement: in the check plan.

## VER-108 - A test-only dispatch instrument ships in production

Reported by check-machinery.

Hidden `--full-scan-min-blobs` in `crates/cli/src/lib.rs` and its entry in `commands/getparents/mod.rs`
force a dispatch arm on small fixtures and are not gated by `test-hooks`.

Fix: an internal policy input exposed across crates only in hook builds, or adopt it as a supported
policy. Enforcement: feature-gated command-tree assertions and visibility.

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

## VER-111 - Requested stop markers can silently do nothing

Reported by bench-verify.

`benches/lib/stop.lua` validates spelling and `profiling.lua` forwards it, and scripts accept a normal
completion when `r.stopped_at` is absent. A renamed marker turns a phase run into a full planet run.

Fix: a shared result helper failing when `stop` was not reached. Enforcement: helper tests for reached,
missing and renamed markers and a rule for stop-capable scripts.
The helper is pbfhogg's; the optional brokkr-side strict `stop_at` is filed as bz brokkr#160.

## VER-112 - Expected answers disappear when lookup keys stop matching

Reported by bench-verify.

`diff-pair.lua` and `diff-applied.lua` get `want = nil` on missing or changed keys and either answer
passes; the integrity benches accept either when absent.

Fix: an assertion mode requiring an expectation and recording whether one applied. Enforcement: reject
orphaned expectation keys in a configuration check. The correct answer still needs an independent oracle.
Brokkr side (orphaned-key check, require-expectation lookup) filed as bz brokkr#161.

## VER-113 - Workload names describe paths scripts never establish

Reported by bench-verify.

`diff-unindexed.lua` never checks that inputs lack indexdata. `dedupe-pair.lua` does not require
passthrough or coalesced copying, and its `--check yes` assumes a subset relation.

Fix: eligibility and coverage predicates checked outside the measured child and from counters afterward.
Enforcement: script contract tests and typed workload declarations.

## VER-114 - The geocode result parser accepts missing evidence

Reported by bench-verify.

`geocode-query.lua` requires only `us_per_query_best`, and the digest covers only admin level and name, so
unchanged counts can hide changed answers.

Fix: a versioned structured result with every field validated and a hash of the complete answer.
Enforcement: parser mutation tests and fixtures with equal counts but different values.

## VER-115 - Every geocode-query iteration rebuilds the whole index

Reported by bench-verify.

`geocode-query.lua` builds per iteration, three full builds by default, all discarded.

Fix: split production from measurement and reuse an immutable index keyed by input digest, producer and
options, owned by brokkr. Enforcement: invalidation tests varying each key component.

## VER-116 - The ranked metric includes work scripts tell readers to exclude

Reported by bench-verify.

`diff-text.lua`, `dedupe-self.lua` and `sort-cold.lua` tell readers to compare the command wall but do not
mark that child `measured = true`, so iterations rank on the whole block.

Fix: mark the child and show setup separately. Enforcement: a shared single-command helper and a contract
test of which call supplies the result.

## VER-117 - The buffered and io_uring merge comparison has a fixed predecessor bias

Reported by bench-verify.

`merge-workload.lua` always runs buffered first, and only buffered ranks the run.

Fix: separate arms and interleaved matched single iterations. Enforcement: brokkr enforcing one backend per
measured iteration and recording order. Equal drive state cannot be proven.
Brokkr side is the existing bz brokkr#129 (interleaved rounds), commented with this case.

## VER-118 - The osmium comparison relies on aligned defaults

Reported by bench-verify.

`vs-osmium.lua` says both tools use zlib level 6 and neither line pins it; strategy and retention also rely
on defaults, and only success is checked.

Fix: rows as explicit operation contracts generating argv, with outputs validated in a separate run.
Enforcement: contract tests for flags and independent output comparison for equivalence.

## VER-119 - The comparison driver has drifted from its table

Reported by bench-verify.

`scripts/vs-osmium.sh` says every row but omits `extract`, hardcodes OSC 4705, offers no dataset selection,
and accepts zero or negative `PAIRS`. The default schedules 63 invocations.

Fix: generate rows from one registry, require positive pairs, and resolve OSC by dataset. Enforcement: a
driver test with a recording brokkr stub.

## VER-120 - Host identity changes coverage and build features

Reported by bench-verify.

Host files differ in datasets, bboxes, OSCs, expectations and features, for example `dm6.toml` versus bygg.
Brokkr adds host features to bench features, so an invocation can change workload and build.

Fix: a host-independent experiment manifest with machine resources separate. Enforcement: required coverage
checked before running, and missing coverage reported as incomplete certification.
Brokkr side filed as bz brokkr#162.

## VER-121 - Cache preparation reaches ambient utilities and shared state

Reported by bench-verify.

Cache benches invoke GNU `dd` and `fincore` from `PATH`, eviction changes shared page cache, and the
residency check is one observation before launch.

Fix: a brokkr operation taking input handles, declaring access, detecting support and recording the
observation. Stable cache state through a run cannot be enforced and should stay a stated limitation.
Brokkr side filed as bz brokkr#163.

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

## VER-125 - The Lua checks do not exercise behavioural contracts

Reported by bench-verify.

Brokkr's bench check compiles scripts and resolves globals, and its manual says wrong keys, types and nils
surface only at runtime. Dry-run executes validation only. All 32 executable scripts are registered today,
and nothing requires a new one to be.

Fix: a script contract harness with controlled `binary`, handles, results, configuration and recording,
plus enforced registry discovery.
Brokkr side filed as bz brokkr#164.

## VER-126 - Repeated bench parameter validation is diverging

Reported by bench-verify.

Unsigned parsing is copied across jobs and region params. Geocode's bbox splitter skips empty fields and does
not check finite values or bounds, and integer validators do not enforce Rust widths, so invalid values can
fail after building.

Fix: parameter types in brokkr's schema or shared constructors. Enforcement: contract tests and a source rule
against local replacements.
Brokkr side (typed param domains) filed as bz brokkr#165. brokkr runs validate() before any build, so "fail after building" holds only for values a script's own check lets through.

## VER-127 - Verify workspace requirement on the pbfhogg verify reimplementation

The verify workspace (see VER-123; brokkr side brokkr#154, closed wontfix).
brokkr verify is to be removed and reimplemented in pbfhogg, so this is a
requirement on that implementation: its outputs go to an owned per-run scratch
directory, with any retained evidence kept separately, never under `target/`.
The rest of the test-filesystem work is done; its settled design is stated
in `reference/testing.md` "Test scratch" and in the leaf crate
`crates/test-scratch`.
