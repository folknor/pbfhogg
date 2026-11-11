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

## VER-033 - Cancellation coverage is a sample of commands and checkpoints

The claims in `crates/lib/tests/cancel.rs` are renamed to what the sweep does, with
its registry exclusions documented. Complete coverage, a command-scenario registry
with explicit modes and exclusions compared mechanically against the command
inventory, remains undone.

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

## VER-086 - Bad-request validation has no mutation coverage

`bad_requests_are_refused_quietly` in `crates/harness/tests/exe.rs` now uses live FIFOs,
a valid control launch and branch-specific refusal reasons. Not done: mutation tests
disabling each validation branch.

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

## VER-100 - Nothing enforces the complete gate at merge time

`reference/testing.md` now names `brokkr check --gate` as the before-merge command and
calls `full` partial. Nothing enforces it: the only workflow, `.github/workflows/docs.yml`,
deploys documentation. Residue: a required merge check, if the project wants one.

## VER-104 - The document checker is not verified against renderer output

`scripts/check_doc_links.py` now recognizes Setext headings and its self-test,
`scripts/test_check_doc_links.py`, runs in the gate. Its GitHub and VitePress slugging, inline rendering and
sidebar parsing are still unverified against real renderer output: expected-output fixtures
from the pinned VitePress version and an independent GitHub reference are not done.

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

## VER-107 - The musl sweeps assume an x86_64 host

Reported by check-machinery.

Both executable musl sweeps in `brokkr.d/checks.toml` hard-code `x86_64-unknown-linux-musl`,
now with comments and a host restriction stated in `reference/testing.md`. On aarch64 they
would be cross-builds with foreign executables. Residue is brokkr-side check planning:
resolve a runnable target from host capability, or refuse an unexecutable target before
building.

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

## VER-125 - Most bench scripts have no contract test

Reported by bench-verify.

`bench_tests` and `bench_scripts` are set in `brokkr.d/benches.toml`, so unregistered scripts are refused
and `benches/tests/` runs on every `brokkr check`. Only ref-integrity and the six page-cache benches have
tests there; every other script's argv, record and verdict paths run only on a real bench.

Fix: a contract test per script for the argv it builds, its stop handling and its verdict.

## VER-126 - Nothing stops a local copy of a typed param validator

Reported by bench-verify.

The integer and bbox params now use brokkr's typed domains, with `benches/lib/bbox.lua` adding only the
strict order pbfhogg's `Bbox::try_new` needs. Nothing refuses a new script that hand-parses an integer or a
bbox again instead of declaring the domain.

Fix: a source rule against local replacements, if one can be stated without false positives.

## VER-127 - Verify workspace requirement on the pbfhogg verify reimplementation

The verify workspace (see VER-123; brokkr side brokkr#154, closed wontfix).
brokkr verify is to be removed and reimplemented in pbfhogg, so this is a
requirement on that implementation: its outputs go to an owned per-run scratch
directory, with any retained evidence kept separately, never under `target/`.
The rest of the test-filesystem work is done; its settled design is stated
in `reference/testing.md` "Test scratch" and in the leaf crate
`crates/test-scratch`.
