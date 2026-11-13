# Testing

Test placement, validation tiers, and conventions for pbfhogg.
Cross-ref `reference/performance/README.md` for perf baselines, and
"Tests that do not run everywhere" below for the runbook on tests
that skip or compile out by default.

## Test placement

Tests go where they couple least.

- **Inline unit tests** in `crates/lib/src/**/*.rs` `#[cfg(test)] mod tests` -
  module internals; die with the module on rewrite, which is the
  point.
- **Platform-dependent tests** (O_DIRECT, io_uring) sit with the code
  they test, in tier 1, like any other test: placement follows cost and
  reliability, never the feature. They carry the feature's `cfg` (on
  the test or on a grouping module such as `mod backends`) and, where
  the host lacks the feature (no O_DIRECT on the filesystem, io_uring
  refused), skip on the typed refusal and pass in ordinary runs.
  Libtest has no skipped outcome and brokkr counts a returning test as
  passed, so a regression that misclassified a real failure as
  `PlatformUnsupported` would turn the test into a passing skip. The
  complete gate closes that for the backends it lists:
  `brokkr.d/checks.toml` sets `PBFHOGG_TEST_REQUIRE` on the
  `gate-tests` profile (comma-separated labels from `direct-io`,
  `io-uring`, `promise-guard`; an unknown or malformed token fails every test that
  consults it), and every refusal predicate that lets a cell skip
  (`common::direct_io_unavailable`, `common::is_uring_unavailable`,
  `CliOutput::is_o_direct_unsupported` and `is_uring_unsupported`,
  `test_support::refused` and its wrappers, `guard::apply` and the
  guard's control test) calls `skip::fail_if_required`, which fails
  the test where the run requires that backend. By convention these
  predicates are the only route to a backend skip; nothing enforces
  that structurally. Tests that assert a refusal itself inspect the
  error directly and are not affected. What the gate guarantees is
  that a required backend's refusal cannot pass as a skip, so it goes
  green only on a host, and a test scratch filesystem, that does not
  refuse a listed backend; sweeps that compile a backend out carry no
  claim for it. The requirement is parsed only when a refusal consults
  it. Each predicate also names the labels that may stop its cell (the
  io_uring writer also needs O_DIRECT; the non-regular-target test
  accepts only `io-uring`), so the requirement is the cell's backend,
  not the refusal's label; the CLI predicates read that label from
  stderr, which cannot turn a required refusal into a skip (an
  unaccepted label is no skip, and the test fails on the error). The
  gate lists `direct-io` and `promise-guard`. It does not list
  `io-uring`: the io_uring writer registers one fixed 16 MiB pool or
  refuses, never a smaller one (the pool is the measured shape, so a
  shrunk pool would be a configuration no bench owns), and the
  distribution default memlock hard limit of 8 MiB refuses it. A gate
  that required io-uring would require a raised rlimit, which a
  standard host does not have.

  Positive execution is witnessed for io_uring only: the io_uring
  cells that write assert that the writer ran, through the counter
  `writer_uring_registered_buffers` (the call's metrics in unit tests,
  the marker stream in CLI cells) or the fault plan's dispatch count,
  so a cell that stops reaching the writer fails. The dispatch count
  exists only in hook builds, so `roundtrip_uring_tiny_output` runs
  unwitnessed in the `release` sweep. O_DIRECT cells carry no such
  witness. The locked-memory allowance is per user and a writer takes
  the whole pool or fails, so every io_uring cell that registers a
  pool (not the non-regular-target test, which is refused before)
  holds `pbfhogg_test_scratch::locked_memory_lease()` (an `flock` on
  the scratch base) across its writer or CLI child, under its
  `HangBound`, so the suite's cells do not compete for it. That
  serializes the suite's live writers; it does not wait for the
  kernel's deferred unpin of a ring closed without unregistering, and
  other processes of the same user are outside it. Environmental
  skips (osmium, namespaces, descriptor limits, `deny_write`) never
  fail. There is no platform module or profile.

  Three shapes cover a capability the host may lack; the host rule in
  `AGENTS.md` (nothing beyond a standard Linux box) decides which
  applies.

  - **Skip on the named refusal, fail on anything else.** A cell skips
    only on the typed refusal its predicate names (a
    `PlatformUnsupported` with an accepted label, a missing osmium
    executable) and fails on every other error, so a misclassified
    failure cannot pass as a skip. The predicate's doc comment names
    the refusals that skip. This is the ordinary platform cell.
  - **Degrade with disclosure.** A tool-side cross-check that needs
    tooling outside the standard host keeps a leg that runs
    everywhere, runs the cross-check only where the tooling is present,
    and says on its output when it did not run, naming what was
    missing. It does not fail on absence: a failing script check
    short-circuits every later phase of `brokkr check`. The example is
    `doc-site` (`scripts/check_doc_site.py`), the VitePress cross-check
    of the always-on node-free `doc-links`. brokkr prints nothing of a
    passing script check, so the disclosure shows on a hand run.
  - **Opt-in instrument.** A test whose point is a specially arranged
    host is `#[ignore]`d with a reason naming the precondition, run
    deliberately with `brokkr test <name>`, and fails, never skips,
    when the precondition is missing: being opt-in already, a skip
    would be a pass that proved nothing. It is named in the `full`
    profile's skip list, which otherwise includes ignored tests. The
    example is `full_registered_pool_instrument` in
    `crates/lib/src/write/uring_writer.rs`: the io_uring boundary and
    pool-cycling writes, run where `RLIMIT_MEMLOCK` holds the pool,
    with every refusal a failure.
- **Library integration tests** in `crates/lib/tests/<topic>.rs` (e.g.
  `crates/lib/tests/roundtrip.rs`, `read_paths.rs`, `edge_cases.rs`) - test
  the library through what it exports; see "What a test may import".
- **CLI integration tests** in `crates/cli/tests/cli_*.rs`, in the
  `pbfhogg-cli` package - drive the `pbfhogg` binary via `CliInvoker`, and
  name the library only through the fixture surface ("What a test may
  import"). Every test that spawns the binary
  lives there and nowhere else: cargo builds the binary for that
  package's integration tests with the test build's own features,
  profile and target, and hands its path as `CARGO_BIN_EXE_pbfhogg`.
  The library package's tests get no such path, so they have no way to
  reach the binary at all.
- **Fault-injection tests** in `crates/lib/tests/fault_*.rs`, modules of `all`
  like every other library test (plus a few unit tests beside the
  code they fault, such as `write::parallel_gzip`'s). Coupled to the
  `pbfhogg::test_hooks` surface by design.
- **External cross-validation** in `brokkr verify`
  (`verify_<command>.rs` modules in brokkr) - compares pbfhogg
  output against osmium / osmosis / osmconvert.

Placement and tiering are independent axes.

## Test binaries

The root `Cargo.toml` is a virtual manifest; the library, CLI and harness
packages live in `crates/lib/`, `crates/cli/` and `crates/harness/`.
The independent library verifier lives in `crates/library/`, and shared
tool config/dataset resolution in `crates/devkit/`. Every
package sets `autotests = false`; every integration-test target is declared
in that package's `Cargo.toml`. One binary per file made every check
sweep link and start one harness per file, and a focused `brokkr test
NAME` list each harness in turn to find one test.

- **`crates/lib/tests/all.rs`** (the library package) mounts every integration
  test file that can share a process as a module, and mounts
  `crates/lib/tests/common/` once. **`crates/cli/tests/all.rs`** (the `pbfhogg-cli`
  package) is its twin for every CLI-driving test: the `cli_*.rs`
  files, `cli.rs`, and `invoker.rs` (the `CliInvoker`), with
  `crates/lib/tests/common/` mounted by `#[path]`. A test's full libtest name is
  `<file>::<path in file>` (`cli_sort::backends::...`) and carries no
  package, so tier filters (`tier2::`, `serial::`, ...) and leaf-name
  filters match in both binaries; a brokkr profile names the target
  `all` plus an `only` filter on the module (`cli_sort::`). Library
  test files gated on a feature carry the `#[cfg]` on their `mod` line
  in `crates/lib/tests/all.rs`, so it alone shows what drops out of the `minimal`
  sweep; the CLI binary has no such gates, since `pbfhogg-cli` always
  links the library with `commands`. **`crates/harness/tests/all.rs`** (the
  `pbfhogg-harness` package) is the third: the tests that drive the
  library harness executable (`exe.rs`) and the real-limit descriptor
  contracts (`nofile.rs`, see below), with `crates/lib/tests/common/` and the
  promise guard (`crates/cli/tests/guard.rs`, mounted at the crate root so its
  control test keeps the name `guard::tests::probe`) mounted by path.
  The harness's own unit tests live in its bin target.
- **`crates/library/tests/process.rs` and `verify.rs`** each own a test
  process because the verifier enables child-subreaper state. Their Rust
  fixtures exercise process cleanup and protocol judgment independently of
  pbfhogg types. Driver unit tests cover strict documents, publications,
  identity and certification; devkit unit tests cover composition and pinned
  dataset resolution. The all/musl sweeps cover these feature-independent
  packages. See [the protocol](library-protocol.md).
- **`crates/lib/tests/fifo_cleanup.rs`** owns its process for the immediate
  BrokenPipe assertion after unwind cleanup. A sibling test's fork can
  inherit the FIFO reader until exec even though it is CLOEXEC, so the
  shared `all` binary cannot establish that no reader remains. A controlled
  fork experiment reproduced a successful write after parent closure and
  BrokenPipe after child reap. The assertion is unchanged; only placement
  removes the unrelated descriptor owner. The capture implementation remains
  shared with harness executable tests.
- **`crates/lib/tests/common/` is shared by path, not by a crate.** Each package
  compiles it with its own features, which carry the same names
  (`linux-direct-io`, `linux-io-uring`), and the hook cfg
  `pbfhogg_test_hooks` reaches every package of a sweep alike, so a `cfg`
  gate on those means the same thing in all three packages. `commands` does not: in
  `pbfhogg` it is on by default, in `pbfhogg-cli` it is a no-op feature
  that a sweep may leave off while the library has it on. A
  `cfg(feature = "commands")` (or `geocode-reader`) gate in a shared
  helper would therefore silently drop CLI tests out of those sweeps, so
  the `no-library-feature-gates-in-tests-common` textlint rule in
  `brokkr.toml` bans both gates under `crates/lib/tests/common/`; gate the `mod`
  line in `crates/lib/tests/all.rs` instead. A helper crate would see only its
  own features, and its dependency on the library would have to keep
  `commands` off for the `minimal` sweep. `crates/lib/tests/common/` holds fixtures
  only; nothing in it spawns the binary. Two files there are not modules of
  `common/mod.rs`, so pbfhogg-cli never compiles them: `fifo_capture.rs`
  and `reopen_cell.rs` (the reopen-strategy admission the `fault_*_input`
  replacement cells share) are each declared once, by `#[path]`, in
  `crates/lib/tests/all.rs`, and the harness mounts `fifo_capture.rs` the
  same way. `reopen_cell.rs` also holds the replacement seam
  `Scenario::run` (`Pinned` or `Path`): it pins the input with an injected
  reopen name (a scratch hard link for `Pinned`, none for `Path`) and hands
  that pin to the closure, so a replacement cell needs no procfs; the
  closure must pass that pin to the command rather than re-pinning the path.
- **A new test file not declared in the `all.rs` under its package's test directory (or as
  its own `[[test]]`) is never compiled.** Add the `mod` line with the file.
  The test-module audit (`crates/source-policy/src/rules/test_modules.rs`)
  reports such a file: every `.rs` under a member's `tests/` must be
  reachable from a `[[test]]` target that some sweep in `brokkr.d/checks.toml`
  builds, following `mod` declarations, `#[path]` attributes and `cfg` gates
  evaluated against that sweep's features. Module presence is checked too:
  every `mod` declaration in a test file, inline modules included, must be
  enabled (its own and its enclosing modules' `cfg` gates true) in at least
  one sweep that builds the file, so a test module gated out of every sweep
  is reported. It is lexical: it reads source
  reachability, not macro expansion or whether a test executes; an external
  `mod` inside an inline module, a `cfg_attr` on a module, or a `cfg` form its
  small evaluator does not know fails closed until the audit is extended.
- **A test that must own its process** gets its own `[[test]]` target
  in its package's `Cargo.toml` with the reason written beside it; the `[[test]]`
  entries are the list. Arming a fault is not such a reason: fault
  state is per run (see "Conventions").
- **What the shared binary shares.** Every module of `all` runs
  concurrently in one process with: `RLIMIT_NOFILE`, which the library
  `all` raises to the hard limit once before `main`
  (`crates/lib/tests/process_setup.rs`; the CLI and harness `all`
  binaries leave the inherited limit alone), and which child processes
  inherit; the real descriptors every test holds; and rayon's global
  pool, which only a test choosing `Executor::Ambient` reaches, since
  every test host carries an executor pool of its own. No fixture or host
  constructor changes process state. The sidecar metric sets (the
  writers', the read pipeline's and the worker pools' stall counters)
  are not shared: each belongs to the `Invocation` a run is given, so a
  test that needs one run's exact count reads it from its own token
  (`Invocation::for_each_counter`, or `metrics()` on a call or permit
  inside the crate),
  never as a delta of anything another test can move.
  A test that needs anything else here to itself takes a target of its
  own.
- **Descriptor-heavy scenarios are admitted, light ones are not
  counted.** A light host (`common::test_host()`, and `Host::for_tests()`
  in the unit-test binary, which neither raises its limit nor gates)
  grants at most `TEST_LIGHT_FD_GRANT` per request, below an external
  join's stage-1 minimum, so a descriptor-heavy run on it refuses typed
  rather than running ungated. A scenario that runs external joins first
  takes one ticket of whole-run units from the binary's gate
  (`common::heavy_ticket`, in `crates/lib/tests/common/fd_gate.rs`) and
  builds its hosts from it (`common::heavy_host`, or a test-local
  provider delegating to the ticket); a test that builds its own
  `host::FdPool` holds a ticket by hand. The gate admits tickets in
  arrival order against the soft limit less `TEST_LIGHT_FD_RESERVE`,
  charging `TEST_HEAVY_RUN_FDS` per unit. A scenario whose units exceed
  what the limit leaves, or that is not admitted within
  `TEST_FD_ADMISSION_BOUND` behind its siblings, skips as environmental
  before it opens anything; on an ordinary host it never waits. What
  this does not bound: a ticket's units bound its simultaneous runs by
  the scenario's construction, which nothing checks; the per-run charge
  is witnessed only for the largest buffered-output external join a heavy
  grant admits (the `process_nofile` target forces every stage-1 fleet and
  the relation scan's input open at once, the most slot buckets, and runs
  it under a kernel limit leaving exactly the charge free), not for Auto
  routing, fault paths, the direct-I/O and io_uring writers or a
  scenario's own fixtures; the light reserve is asserted, not measured,
  since a process-wide descriptor count in this shared binary includes
  every concurrent test and light use grows with the runner's thread
  count; light tests'
  descriptors, and every command that requests none, are not counted;
  and a test holding a ticket must not take another (it waits on itself
  and skips).
- **Process-wide panic hook.** The reader pipeline-death unit test temporarily
  swaps the panic hook to silence its deliberate worker panic. While the
  replacement is installed, it can also suppress panic output from another
  concurrently running unit test. An integration test in `all` cannot swap
  the hook at all; one that needs a deliberate unwind on a pool thread
  (where libtest's output capture does not reach) to stay quiet raises it
  with `std::panic::resume_unwind`, which unwinds without running the panic
  hook (the panicking-sink test in `host_contract.rs` does).
- **Fault hooks are per run, so fault tests share the harness.** No
  fault hook is process-global: their state lives in the fault plan
  carried by the run's cancellation token (see "Conventions"), so an
  armed fault reaches only the run handed that token, and a fault test
  runs concurrently with everything else in `all`. The plan's sealed
  constructor, the library static inventory in `crates/source-policy`,
  and the textlint rules in `brokkr.d/lints.toml`
  (`no-static-fault-hook-state`, `fault-hooks-only-in-test-hooks`,
  `no-static-test-hosts`) keep it that way.

The `pbfhogg-cli` program is its library target (`crates/cli/src/lib.rs`,
`pbfhogg_cli`); the `pbfhogg` bin is only `main` calling
`pbfhogg_cli::run` and sets `test = false`. The library target exists
so the package's tests reach the command tree production parses,
`pbfhogg_cli::cli_command()`: the unit tests in `crates/cli/src/` walk it, and
the command-table tests in `crates/cli/tests/` take their subcommand lists
from it (`invoker::subcommands`, `invoker::subcommand_paths`), never
from `--help` text. It is not an API and sets `doctest = false`.

The tested sweeps that select the CLI and harness packages (their packages,
features and targets are in `brokkr.d/checks.toml`, the one inventory) run
the CLI tests against a binary with the hook cfg and both platform
features, one without the platform features (pbfhogg-cli's default build),
and the same two shapes again for musl. No sweep pre-builds the binary
(`build_packages`): cargo builds it for each test build. The `minimal`
and `geocode-reader` sweeps stay library-only: the CLI's lib dep would turn
`commands` back on. The `geocode-reader` sweep runs
`crates/lib/tests/geocode_reader.rs`, declared under that feature in
`crates/lib/tests/all.rs`: reader contracts on index records it encodes
itself, with no command builder, so they keep running without `commands`. `brokkr test NAME` searches one package,
`[test] default_package = "pbfhogg"`; a CLI test needs
`brokkr test -p pbfhogg-cli NAME`.

Every `pbfhogg` child the CLI tests start goes through `CliInvoker`, and
on Linux (little-endian x86_64 or aarch64, with
`SECCOMP_RET_KILL_PROCESS`) runs under the promise guard
(`crates/cli/tests/guard.rs`): it inherits a pipe descriptor as a caller's
lease, and a seccomp filter kills it with `SIGSYS` if it starts a
process or closes, duplicates, passes or reflags that descriptor, at
any point in its run. `CliInvoker` turns such a death into a failure
naming the guard, so every CLI test doubles as a behavioural test of the
promise in `docs/cli/commands.md` "Names pbfhogg leaves behind". The guard is
stricter than the promise (no duplicate of the lease, no `sendmsg`,
`sendmmsg` or `pidfd_getfd` at all) and blind to io_uring submissions
and to a `clone3` caller that ignores its `ENOSYS`; the module doc says
why. The instrumented CLI and harness run under it in the profiling lane
(below). The
`no-hotpath-cpu` script check in `brokkr.d/lints.toml`
(`scripts/check_hotpath_cpu.py`) covers every profiler build by failing
when the resolved dependency graph, with every workspace feature on,
enables hotpath's process-starting CPU profiler. `guard::tests` re-runs the test binary under the guard once per
rule to prove each one fires and that the allowed neighbours do not. A
test that starts the binary some other way escapes the guard: do not.
The library worker launched by the library-scenario bench inherits brokkr's lease the same way and keeps the
same promise; every child `crates/harness/tests/exe.rs` starts runs under the
same guard, mounted from `crates/cli/tests/guard.rs`. The independent
`pbfhogg-library` verifier uses its own subreaper/pidfd cleanup; the harness
has no supervisor. Verification is
`brokkr run --with pbfhogg-harness pbfhogg-library -- ...`, and measurement
is `brokkr bench library-scenario` (see `reference/library-protocol.md`).
On any other architecture the filter is compiled out: children run
unguarded and the control test skips through `skip::backend_unavailable`
with the reason (as it does on a supported architecture whose kernel
refuses `SECCOMP_GET_ACTION_AVAIL`, errno included); where the run
requires `promise-guard`, `guard::apply` and the control test fail
instead. That fallback is
type-checked by the `riscv64` lint sweep in `brokkr.d/checks.toml`, through
the libc-only fixture crate `crates/guard-portability`, which mounts the
guard and nothing else; no sweep compiles the rest of the workspace for an
unsupported architecture.

## Validation tiers

Runtime-ranked. Each tier subsumes the cost of the previous; higher
tier = more expensive = run less often. `#[ignore]` is only a Cargo
mechanism, not the tiering model.

| Tier | Cost | When | Driven by |
|---|---|---|---|
| 1. Fast contracts | seconds | Every edit | `brokkr check` (default) |
| 2. Command slice | tens of seconds | While working on that command | `brokkr check --profile <cmd>` |
| 3. Full in-project | minutes | Once, at the end of a campaign | `brokkr check --gate` |
| 4. Scale/perf | hours | Performance work, release evidence | `brokkr bench`, `brokkr suite` |
| 5. External cross-validation | depends | Release gate | `brokkr verify` |

Tiers 1-3 use `brokkr check`. Tiers 4-5 are separate brokkr commands.
`brokkr check --gate` runs the unfiltered feature and target sweeps,
doctests, execution accounting and gate-only member checks. It runs once,
at the end of a campaign of changes, not per commit: each commit lands on
a green `brokkr check`. The `full` profile is partial: it selects only
`all` and has substring skips. Focused profiles, including `full`, do not
replace the gate, even when they report success.

The root `brokkr.toml` enables the rustdoc phase with `[rustdoc]`.
It resolves intra-doc links per distinct build shape after clippy, before
pre-test checks, and fails on warnings as well as errors. Markdown link
checking does not replace this compiler pass. The complete gate requires
this phase; a partial profile may skip it.

The `serial` profile selects `all-serial`, the non-parallel twin of `all`
curated to the `serial::` modules, and sets `test_threads = 1`. Both binary
execution and tests within each binary are sequential. Keep the twin's
packages and features aligned with `all`; a thread limit on a parallel
sweep cannot provide this guarantee. The gate does not provide it either:
a complete lane may not skip tests, so the gate runs the `serial::`
modules inside the parallel `all` lane, concurrently with other binaries.
A test that is wrong, not merely slower, under concurrency does not
belong in `serial::`; that module is for tests whose cost (memory, for
instance) makes concurrent runs undesirable, and `brokkr check --profile
serial` is how they get their sequential run.

The executable `musl` and `musl-all` sweeps currently require an x86_64
Linux host able to execute `x86_64-unknown-linux-musl` binaries directly.
Their literal targets do not adapt to aarch64 hosts. The `riscv64` lint
sweep is compile-only coverage of the guard fallback, not executable
musl coverage. Brokkr refuses configured target runners, but needs a
planner capability check to refuse an unexecutable target before building
or resolve a native musl target. Until then this is a host restriction
of the configured gate, not a portable execution guarantee.

Three further tested sweeps run in the gate only. `release` runs the
library and CLI suites in the shipped release profile (fat LTO, debug
assertions and overflow checks off), default features, no hook cfg, so a
`#[should_panic]` invariant test whose panic came from a debug-only check,
or a path whose arithmetic wraps only in release, fails there; every other
tested sweep builds dev. It builds both platform backends in, the shape
brokkr benches, so the O_DIRECT and io_uring tests run with release
semantics under the gate's backend requirement; the feature-off refusals
run in dev only (consumer, musl). `harness-hotpath`,
`harness-hotpath-alloc`, `cli-hotpath` and `cli-hotpath-alloc` are the
profiling lane (`brokkr check --profile profiling` runs it alone; operator
profiling uses `brokkr bench --hotpath library-scenario --scenario hotpath-cat
--dataset denmark`): the
harness and CLI suites built with each profiling feature. Both executables
profile only when `HOTPATH_OUTPUT_PATH` asks for a report, so every
unprofiled test checks an instrumented build quiet, under the promise
guard; harness describe mode ignores a profiling request; and a profiled
launch must write a report naming library functions with calls and, under
`hotpath-alloc`, allocated bytes counted, leaving the CLI's stdout
byte-identical (`crates/cli/tests/cli_profiling.rs`).

Gate-only lint sweeps compile and lint what no tested sweep carries: the
`hotpath` and `hotpath-alloc` shapes over every package, `musl-hotpath-alloc`
(the profiling code on musl, where a glibc-only allocator call would fail),
and the mixed backend shapes (one of `linux-direct-io`, `linux-io-uring`
without the other) on both targets.

`brokkr check` enforces a fixed 20 s per-test watchdog with no
override, on every profile including `--profile full`, so no test may
run longer than that. `brokkr test --timeout` exists for debugging a test
that overruns, and nothing is designed around it. `[test.profiles.full]`
in `brokkr.d/checks.toml` includes ignored tests, and its by-name `skip`
list holds only the opt-in instruments (see "Opt-in instrument"), each
run individually via `brokkr test <name>` (`brokkr test -p pbfhogg-cli
<name>` for a CLI test) where its precondition holds.

A test's own failure bounds (the hook wait, the hang bound of the timeout
helpers, the observation bound, the harness rendezvous, the child timeout)
are the "Test time bounds" in `crates/lib/src/tunables.rs`, const-asserted
below a mirror of that watchdog so a broken run fails with a named message
before the watchdog kills it unnamed; a test names one of them, never a
literal duration of its own. The observation bound (`TEST_OBSERVE_BOUND`)
is for a transition a correct implementation makes at once: a thread
reaching the point it was set up to reach, a child ending at once after a
signal. A wait whose defect
would be a hang is a `pbfhogg_test_scratch::HangBound` armed at
`TEST_HANG_BOUND`, declared first in the test so it also bounds the release
guards and joins after it, with the operation on the test thread: a hung
thread cannot be joined, so on expiry it writes one line naming the test to
stderr and aborts the test binary rather than panicking past a thread left
running under later tests (brokkr reports the binary killed by signal 6
while running that test). A test whose waits are several in sequence
carries one such bound over all of them. Abort orphans no child: every test
child carries a parent-death `SIGKILL` (`common::process::isolate`; the
hang bound's own expiry probe in `crates/test-scratch`, which cannot reach
`isolate`, arms the same signal itself), keyed
to the thread that spawned it, which is why a test child is always spawned,
owned and reaped by its test's own thread. No test throws away a
`thread::spawn` handle (the source-policy detached-threads rule, a lexical
tripwire), and a test joins every thread it starts on every exit, a failed
assertion's unwind included: whatever the thread is parked waiting for is
released before the join (a release sender owned inside the
`std::thread::scope` body, a guard, or
`test_support::with_parked_thread`), and the join sits under the test's
`HangBound`. A rendezvous that cannot be released (a `std::sync::Barrier`
whose other party failed before arriving) leaves the `HangBound` as the
fail-stop. A scoped thread that only runs an operation, waiting on nothing
the test or a sibling supplies, is parked on nothing and owes neither a
release nor a bound, like the same operation on the test thread. The rule
cannot see a named handle dropped on an unwind, so
it rests on review. A test never infers that
another thread blocked from elapsed time: it waits (bounded) for an
observation made at the wait itself - a `test_support::Parked` count under a
primitive's mutex, a fault-plan hook at a join, a lock or an empty delivery
window, a slow-path tally - and only then releases it.

A time value survives only as a deadline bounding a wait that normally ends
on an event. A wait on something our code controls ends on a notification
(the producer signals; a test gates the producer instead of sleeping it,
and a child announces its pause on a pipe instead of the test polling for
a file); a timed wait survives only where the other side cannot notify, and
says why. The source-policy timed-waits rule
(`crates/source-policy/src/rules/timed_waits.rs`) enforces the spellings:
a `sleep`, `sleep_until` or `sleep_ms` identifier, libc's `nanosleep`,
`clock_nanosleep` or `usleep`, or a spin primitive named by its path
(`thread::yield_now`, `hint::spin_loop`), anywhere in a member's `src/` or
`tests/`, production included, is refused with no exemption. A test's own
state that another thread waits on rings a `pbfhogg_test_scratch::Bell`
after each store (the fault plan's bell protocol: snapshot the ring count,
evaluate, sleep only while it is unchanged, one deadline); a library slow
path a unit test observes rings a `counters::TestBell`, zero-sized outside
test builds. Every other std timed wait (`park_timeout`, `recv_timeout`,
`recv_deadline`, condvar `wait_timeout*`) and each libc raw wait the tree
uses (`libc::poll`, `ppoll`, `epoll_wait`, `select`, `pselect`; an
inventory, not every timed syscall) needs a `// wait-deliberate: <reason>`
line comment on its line or alone above it, except a
`recv_timeout`/`recv_deadline` method call followed directly by `.unwrap()`
or `.expect(`, whose expiry panics at the receive. The duration-literals
rule (`crates/source-policy/src/rules/duration_literals.rs`) refuses a
`Duration` built from a numeric literal outside `tunables.rs` (and the hang
bound's own tests in the test-scratch crate, which cannot name the
library's tunables), unless `// fixture-time: <reason>` says the value is
data, not a wait. A poll needs none of those names, so the loop-waits rule
(`crates/source-policy/src/rules/loop_waits.rs`) covers the loop itself:
a `loop`, `while` or `for` whose own extent names `try_recv`, `try_lock`,
`try_wait`, `try_send`, `Instant`, `elapsed`, `duration_since` or
`saturating_duration_since`, or calls `.load(`, needs a
`// loop-ends: <reason>` comment on its keyword line or opening the run
of line comments directly above it,
saying what wakes or ends each pass (the blocking call and its notifier, or
that the loop never waits and how it progresses). The obligation reaches
every enclosing loop; a `fn` item nested in the loop is not its extent; an
atomic `.load(` in a `for` whose header spells no endless source (`0..`,
`repeat`, `cycle` and kin) reads data and passes. Lexer tokens find the
keywords and probes and syn only narrows: a keyword syn does not parse as
a loop is exempt unless it sits in a macro or attribute, where the whole
opaque region is its extent, and a file syn cannot parse fails the rule.
All three are spelling tripwires: wrappers, named integers, code a macro
assembles from elsewhere, a bare imported `yield_now` and a hand-written
endless iterator are outside them, and a marker's reason is reviewed, not
checked.

**No test reads a downloaded dataset.** Every test builds its own
input in scratch. Real-data coverage (Denmark, planet, replication
diffs) is `brokkr verify` and `brokkr bench`, which resolve datasets
per host from `brokkr.toml`; the test suite must pass on a checkout
with an empty `data/`.

**No test depends on the host.** No goldens blessed on one machine, no
core-count, memory or rlimit preconditions. `common::test_host()` and,
for unit tests under `crates/lib/src/`, `Host::for_tests()` hand the library a
`host::Fixed` (a fixed core count and memory budget), an executor pool
of its own of the same four-thread width, and a descriptor provider
granting every request `min(wanted, TEST_LIGHT_FD_GRANT)` (a heavy host
built from a gate ticket: `min(wanted, 1024)`) without probing the process,
so what the library is told and where its rayon work runs are the same
everywhere and independent of sibling tests; a test that needs a
different machine sets its own `host::Fixed`. CLI children are told the
same machine through `CliInvoker` (see its entry below), in builds with
the hook cfg. `Executor::Ambient`
appears in a test only where it is the subject, marked
`// ambient-deliberate:` on its line. Each test owns its hosts and
invocations: none is held in a static or leaked (`Host::entered` borrows
an entered view from the test's own host). The real descriptor limit
is shared, as "What the shared binary shares" says, and gated for
descriptor-heavy scenarios only. Tests may still skip where
the host lacks a feature (O_DIRECT, io_uring, osmium in PATH,
unprivileged user namespaces) or where the descriptor gate does not admit
a heavy scenario.

**Small configurations come through existing seams, not tunable
overrides.** The constants in `crates/lib/src/tunables.rs` get no runtime or
test-only override by default. A test that needs a small configuration
uses the host's resources and `-j` for worker counts, the engine's own
parameters (the ordered pool's `PoolConfig`, the parallel reader's
private budget and batch sizes) or a crate-private dispatch function; a
constant that blocks a named test gets a narrow private parameter at the
mechanism that owns it (merge-changes' streaming `Batching`, apply-changes'
`write_run` batch). A cross-crate test instrument is hidden, unsupported
and compiled only under the hook cfg: getparents' full-scan threshold, and the fault plan's
nonzero scheduling overrides (`diff_unit_target_bytes`,
`diff_unit_lone_elements`, `diff_text_chunk_bytes`,
`diff_text_overflow_bytes`, `derive_chunk_bytes`,
`apply_changes_create_batch_blocks`), which a library integration test sets
on its own invocation. A test that shrinks a threshold this way keeps one
test of the production value at a size it acts on, in `tier3`. The module
doc of `crates/lib/src/tunables.rs` gives the reasons.

**No checked-in input data.** Fixture PBFs are built at test time into
scratch by the `crates/lib/tests/common` writers (`write_test_pbf*`,
`write_multi_block_test_pbf`, `write_base_pbf` and its `changed` and
`bad_indexdata` variants) from the `generate_*` element generators, with
`common::adversarial` for byte-level damage. One exception:
`crates/lib/tests/test.osm.pbf` is the input the library's doc examples and guide pages
open by name, so an example a reader copies stays short and real. It is small,
host-independent and
regenerated by `crates/lib/examples/gen_test_pbf.rs`; nothing else may be checked in,
and new tests build their own input.

Every element fixture writer in `common` encodes through one
`encode_fixture_blocks` and frames through one `frame_fixture_blocks`, so
writers that differ in framing (sort flag, BlobHeader indexes, sync or
pipelined writer) write the same blocks by construction;
`crates/lib/tests/oracle_model.rs` checks paired fixtures frame for frame.
A fixture of pre-encoded blocks (hand-built wire bytes included) frames
through `frame_fixture_blocks` too. A test whose input is defined by what
`FixtureFraming` cannot state - header fields beyond the sort flag,
per-blob indexdata, a compression, an element order or encoding the shared
encoder does not write - builds it with `PbfWriter` directly; the shared
pair is for fixtures that differ only in framing, not for every input.

**The output oracle decodes independently.** `read_normalized` and
`assert_elements_equivalent` hold the decode of `crates/lib/tests/common/model.rs`,
a hand protobuf decoder that shares no code with pbfhogg's reader, and also
require pbfhogg's public view of the same file to agree with it. The model
keeps nanodegree coordinates, embedded way locations, tags in stored order
with duplicates, unknown member kinds with their value and metadata claims
per field with millisecond timestamps, and refuses any string index it cannot
resolve. A command allowed to rewrite a claim is tested against its expected
output, never through a weaker comparison. Under the same checks,
`read_in_file_order` gives the elements in wire order (a group's merged
DenseNodes run at its first field; pbfhogg's reader iterates a group dense
nodes first, so the two differ only inside a group mixing element fields),
for a test whose subject includes the order written, and `read_raw_meta`
gives each element's metadata fields as stored, before any claim rule, for a
test of what a writer encodes (a dense -1 written as 0), which claims cannot
show. A test reading output through pbfhogg's own accessors is one whose
subject is those accessors (decode-path parity, a codec roundtrip), and it
reads them losslessly. The `test-oracles-no-error-to-empty` text rule is a
tripwire over every test tree for three spellings of an error read as empty
(`unwrap_or("")`, `Result::ok`, the infallible `.tags()`); it is not this
rule, which it cannot see whole.

`mod serial` is an orthogonal config overlay, not a tier. Same for `#[ignore = "external"]` (escape hatch for in-tree
osmium checks).

### Development contract

- **During refactor:** edit code + inline tests + any library
  integration or fault test that imports what moved. Run `brokkr check`. Green ⇒ refactor
  structurally landing. Tiers 2-5 stay silent during iteration.
- **Before each commit:** require a successful `brokkr check` after the
  last edit. Use command profiles for focused feedback.
- **End of a campaign:** run `brokkr check --gate` once over the landed
  commits; command profiles do not replace it.
- **Before release:** run tier 4 for performance regression and
  tier 5 for external parity.

Tier 1 must be both fast AND structurally complete: every internal
contract a refactor could break needs a tier-1 test (inline unit OR
fault hook).

### What a test may import

The boundary follows what a test is for, not one list for every test.

- **Library integration tests** (`crates/lib/tests/`, the harness tests)
  test the library, so they import what it exports: the supported API,
  and also the doc-hidden command entry points (`pbfhogg::altw`,
  `pbfhogg::cat`, ...), `pbfhogg::host`, `pbfhogg::process` and, under the
  hook cfg, `pbfhogg::test_hooks`. The doc-hidden items and the hooks are
  test seams, not supported consumer API: a test reaching them does not
  make them a promise to library users. A test couples to whatever it
  imports and changes with it in a refactor; nothing makes these tests
  immune to internal rewrites.
- **CLI tests** (`crates/cli/tests/`) test the binary. They name the
  library only to build inputs (the block builder and writer), to read
  outputs (the blob and element readers and their view types, the geocode
  reader), to spell the binary's test-hook protocol and to name the two
  test time bounds a CLI test waits on (the child timeout and the
  observation bound), so a rewrite of a
  command's library code cannot break them by type changes alone. Any other
  library path carries a `// library-deliberate: <reason>` comment on its
  line, alone on the line above, or on the first line of its `use`
  statement. The test-import audit
  (`crates/source-policy/src/rules/test_imports.rs`, with its fixtures)
  holds the surface's one inventory (the allowlist the CLI test files'
  module docs name) and refuses an unmarked path outside
  it, parsing every `use` tree. It is a direct-spelling guard: it does not
  see `crates/lib/tests/common/` (mounted by path), re-exports through
  `crate::` or macro expansion. A library call must never stand in for the
  binary under test, and reading output through a library reader does not
  make that reader an independent oracle; "The output oracle decodes
  independently" is the rule for that.

### Per-command CLI test split

Each `crates/cli/tests/cli_<command>.rs` splits intent inside the file:

- **Root or `tier1` module**: small CLI contracts; runs in
  `brokkr check`.
- **`tier2`**: expanded matrices, `-j` parity, adversarial
  fixtures, scratch cleanup, command-local fault injection.
- **`tier3`**: slow self-contained correctness, larger fixtures.
- **`backends`** (optional): groups the `--direct-io` / `--io-uring`
  tests under their shared feature `cfg`. Still tier 1; see "Test
  placement".
- **`serial`**: requires `--test-threads=1`. Fault injection is not a
  `serial` case: its state is per run.

`crates/cli/tests/cli_sort.rs` is the canonical template for new `cli_*.rs`
files.

### The process environment

A test's inputs are what it builds, never what the runner happens to
export. No code in the tree mutates the process environment (`set_var`,
`remove_var`, `set_current_dir`, libc's `setenv` family), and none
enumerates it or reads it except at a closed inventory of reads, each
naming its variable and the reason it is admitted. Every test child starts
from an empty environment (`common::process::isolate`, or `env_clear` in
the leaf crate that cannot reach it) and gets only what its test sets.
The source-policy rules `environment` (the read inventory, the refusals and
the import spelling) and `host_independence` (every child constructor
clears) hold the tree to this; the inventory, with each entry's reason,
lives in `crates/source-policy/src/rules/environment.rs`.

The admitted reasons are four. A **runner requirement** is a declared
input that can only make a run stricter, strictly parsed:
`PBFHOGG_TEST_REQUIRE`, which the gate profile sets so a refused backend
fails instead of skipping, is the only one; a runner input that changed a
fixture, an expected result or an execution parameter would not qualify.
A **host tool** selection is the osmium oracle's search of `PATH`: its
absence skips, and the parent resolves the tool to an absolute path that
the cleared child is started by. A **parent-set** read is a re-executed
test binary reading the gate its parent wrote into the child's
environment. A **protocol** read is a production CLI, harness or tool-boundary input
(`BROKKR_*`, `HOTPATH_OUTPUT_PATH`, the `PBFHOGG_TEST_*` hooks), which a
CLI test sets through a named `CliInvoker` setter. The independent driver
reads `BROKKR_WITH_EXE` and explicit config/host selection inputs, passes
values into devkit and clears worker environments, including `HOTPATH_*`.
The worker reads its artefact directory from argv; only the marker FIFO
remains a `BROKKR_*` worker input. The library reads
nothing (ADR-0016). Compile-time `env!` reads are build inputs and outside
the rule.

### Tests that do not run everywhere

- The O_DIRECT and io_uring tests exist only in the builds with
  `linux-direct-io` and `linux-io-uring` (which sweeps those are is in
  `brokkr.d/checks.toml`) and skip and
  pass on a typed platform refusal, except where the complete gate
  requires the backend (see "Platform-dependent tests"): O_DIRECT where the scratch
  filesystem rejects it, io_uring when setup fails for an
  environmental reason, such as a kernel without io_uring or an
  `RLIMIT_MEMLOCK` below the writer's registered pool (the
  distribution default is).
- Fault-injection tests are neither `#[ignore]`d nor serial: their
  faults ride on the invocation's cancellation token, so they are
  ordinary modules of the `all` binaries and run with
  `brokkr test <name>` like any other test.

## External cross-validation

Lives in `brokkr verify`, not the in-tree test crate:

- The `VerifyHarness` template (`run_pbfhogg`, `run_tool`,
  `diff_pbfs`, `check_sorted`, dataset config, variant matrix,
  results storage) already exists. Each new comparison is
  `verify_<command>.rs`, ~50 lines.
- External tools (osmium, osmosis, osmconvert) are operationally a
  brokkr concern. Contributors don't need them installed for clean
  `brokkr check`.
- An in-tree `mod external` tier would duplicate brokkr's verify
  machinery and act as a gravity well.

The shared `crates/lib/tests/common/oracle.rs` runner resolves osmium to an
absolute executable before changing cwd, reports version probes, sets cwd and
TMPDIR inside borrowed scratch, and uses bounded child and capture ownership.
Only a missing executable permits a skip; other resolution or launch errors fail.
The harness executable tests use the same ownership primitive with their own
launch policy and explicit protocol environment, independently of CLI replay.

**In-tree osmium checks are migration candidates.** Each is
`#[ignore = "external"]`, skips where osmium is absent, and exists
because its handcrafted fixture reaches a path a dataset run does not,
or asserts what `brokkr verify` does not. Migrating one means carrying
over its fixture coverage and its assertions; verify accepting its
inputs is not enough:

- `crates/cli/tests/cli_sort.rs::sort_cross_validate_osmium` - overlapping-blob
  fixture; `brokkr verify sort` on a configured dataset runs against
  real data only and doesn't exercise the streaming sweep merge's
  overlap-run path. `brokkr verify <command> --input <path>` already
  accepts a handcrafted PBF; the missing piece is an overlapping-fixture
  builder under `crates/lib/examples/`, after which the comparison moves to
  `brokkr verify sort --input`.
- `crates/cli/tests/cli_digest.rs::osmium_reencodings_digest_equal` - pins the
  encodings a real dataset run does not vary.
- `crates/cli/tests/cli_apply_changes_osmium.rs` - `apply-changes
  --locations-on-ways` against osmium on a base and change file built so
  that both tools' documented semantics coincide (osmium's location
  index has ordering defects that decide its output otherwise; the
  module doc lists the fixture rules). `brokkr verify merge --input
  <base> --osc <change> --locations-on-ways` can run this fixture, and
  `verify all` runs `apply-changes -l` on a dataset's
  `altw-keep-untagged` base. Both hold pbfhogg's output to
  `check --locations` and to osmium's elements, which ignore inline
  coordinates; neither compares each surviving way's refreshed
  coordinates with the ones computed from the change file, nor the
  refresh-routing summary the test asserts. The test stays until verify
  carries equivalent assertions.

**Escape hatch.** A contributor mid-PR can write an osmium check
next to a fixture for the duration of that PR as
`#[ignore = "external"]` in-tree with a runbook comment, then
convert to a `verify_*.rs` PR against brokkr afterward, once verify can
carry the check's assertions (until then it is a migration candidate
above, with its reason stated). Permitted
exception, not a tier - should not survive across many PRs.

## Conventions

- **`pbfhogg_test_hooks` compiler cfg, not a Cargo feature.** Every
  test hook (the library's fault plans and observers, the CLI's
  `PBFHOGG_TEST_*` environment hooks, getparents' hidden threshold flag)
  compiles only under `cfg(pbfhogg_test_hooks)`. No member declares a
  feature for it, so no feature selection, dependency edge or feature
  unification, a registry `cargo install` included, can put hooks in an
  executable; only an explicit `--cfg pbfhogg_test_hooks` in rustflags
  can, in debug or release alike. The workspace declares the cfg in its
  check-cfg list, so a misspelled predicate is a diagnostic. The hook
  sweeps in `brokkr.d/checks.toml` pass it as `rustflags`. The
  rustdoc phase does not receive it (brokkr carries no sweep `--cfg`
  into rustdoc's flags), so doc comments on hook-only items are not
  linted. The test-module audit reads the sweeps' rustflags
  for it, so a hook-gated test module counts as covered only while some
  sweep sets the cfg.
- **Fault plans ride the token.** `pbfhogg::test_hooks` is the one
  answer to "what can a test inject". Its `FaultPlan` holds every
  token-scoped hook (arming fields and observation counters, disarmed as
  a `MAX` sentinel); one lives in each
  `host::Invocation` and `Invocation::faults()` reaches it. No other plan
  can exist: its constructor takes a witness only `host` can build and it
  has no `Default`, so a test reaches a plan only through an invocation it
  owns. The token is what
  an invocation already hands every worker loop it starts (the read
  pipeline, the output writers, the diff and derive work units, altw's
  stages, the geocode passes), so a new hook is a `FaultPlan` field
  read at the site through the token in hand, gated on
  `cfg(pbfhogg_test_hooks)`. A test arms the plan of the token in
  the `Host` it passes; `common::test_host()` mints a fresh token per
  call. One token is one fault scenario: everything run under it shares
  its plan (sequence numbers, worker indices, counters), so an armed and
  an unarmed run use two tokens. A reader takes its token through
  `ElementReader::invocation`; a public path constructor mints its own,
  reached through `PbfWriter::test_faults`. Per-token isolation does not
  stop an armed run's own parked workers: a test that parks them still
  releases and joins on every exit path. Output publication's hooks are
  on the plan too: `FaultPlan::arm_publish_fault` arms a `PublishFault`
  and `FaultPlan::on_publish_step` observes every `PublishStep` (see
  Publication hooks below). Per-object hooks stay with their owner and
  are listed in the module docs: the geocode reader's counter, and the
  cancellation hooks below.
- **Hook waits ring a bell, they do not poll.** Arming fields and plain
  counters are public atomics. A field that some wait reads is a
  `Watched` atomic instead: anyone loads it, only `test_hooks` writes it,
  and every write rings the plan's bell, so a parked hook or a waiting
  test wakes at the transition. A code path that moves such a field calls
  an `on_*` hook on the plan; a test releases parked work through a
  `release_*` method; `Invocation::cancel` rings too, and so, in hook
  builds, do the merge-changes ledger's stop guard and the altw router's
  abort. No wait polls. A test waits with `FaultPlan::wait_for`, never its
  own loop: it is bounded by `TEST_HOOK_WAIT`, returns false for the
  test's assertion to name, and wakes only on a ring, so its condition
  reads only rung state; a test's own state rings a
  `pbfhogg_test_scratch::Bell` of its own. The bell
  is a notification, not a snapshot: a handshake that needs several
  fields read together, or a transient state seen, keeps its state under
  one lock, as the decode gate does.
- **Cancellation hooks.** `Invocation::on_check` (per-instance, hook cfg)
  runs a callback with each checkpoint's 1-based ordinal and may block;
  `Invocation::checks_passed` counts them. Library tests trip the token from
  the callback (`crates/lib/tests/cancel.rs`): its registry gives every
  public library command module at least one fixture scenario and names
  the modes it leaves out, is checked against the module declarations,
  and each module gets its own sweep tests (tripped before the start,
  sampled ordinals capped at 64 per scenario, census), so no test's time
  is the whole registry's. The CLI, built with the hook cfg, reads
  two variables: `PBFHOGG_TEST_CANCEL_COUNT=<file>` writes the run's
  checkpoint count there at exit, and
  `PBFHOGG_TEST_CANCEL_PAUSE=<k>[:stuck]` parks the thread reaching
  checkpoint `k`, announces `paused <k>` on the descriptor named by
  `PBFHOGG_TEST_ANNOUNCE_FD` (a pipe the test hands down; `CliInvoker` sets
  both), and blocks on the fault plan's bell until the token trips;
  `stuck` announces `cancelled <k>` once tripped and parks for good, so
  only a second signal ends the run. The interruption tests in
  `crates/cli/tests/cli_output_contract.rs` count a command's checkpoints, park it at
  sampled ordinals (tier 1) or at every ordinal of the counting run (its
  `tier3` shards, run by `--profile full` and the gate), signal it, and
  assert the exit code and an empty directory, with no sleeps; a command
  that writes no output is interrupted with the arguments its `NO_OUTPUT`
  entry carries, so each surface is compared against its own inventory
  (library modules, clap subcommands) rather than against the other's.
  Ordinals are
  positions, not sites, and the fixture's counts are not real data's, so
  neither sweep proves every checkpoint site of a planet run unwinds.
  A row whose checkpoint layout would depend on scheduling is pinned to
  one schedule rather than waived: the counting run and every paused run
  go through the same per-row setup (`with_visible_scratch`), and each
  row must observe both a populated staging entry and, where it takes
  `--tmp-dir`, scratch payload (only the sparse add-locations-to-ways row
  is excused, its store being unlinked once mapped).
  `CliInvoker::spawn` returns a `RunningCli` that can be signalled and is
  killed, reaped and its cancellable capture readers joined on drop.
- **Merge-changes spill.** The ordered merge spills a later input's
  output only when scheduling lets it run ahead of the head, so
  `FaultPlan::merge_changes_spill_first = N` forces it: input 1 spills its
  first `N` members before input 0 is read, and its reader holds before
  batch `N` until input 1 is the head, so a promotion copies back a spill
  file that is still open (more than `N` batches) or one already closed
  (at most `N`). It needs two inputs and two readers; a handshake that
  does not happen within the hook bound fails the run, so a subprocess
  cannot pass unarmed. `--simplify` is not affected. The CLI, built with
  the hook cfg, arms it from `PBFHOGG_TEST_MERGE_CHANGES_SPILL_FIRST=<n>`
  (a value that does not parse exits 101); the two-input merge-changes
  row of the interruption sweeps runs under `1`, and `crates/lib/tests/cancel.rs`
  arms it for its merge-changes entry.
- **Capability accounting.** `Invocation::capabilities` (hook cfg,
  doc-hidden) reads the invocation's transition word (live call guards and
  permits, and whether a count saturated) and its census (issued block
  iterators, writers, bound outputs and blocks). `crates/lib/tests/cancel.rs`
  asserts that it is back to all zero after every run of its fixture
  scenarios, sampled cancellation runs included; that each
  scenario's census callback observes at least one checkpoint and sees no
  value the command issued itself at any observed checkpoint; and that a
  nested public entry mints its own guard; `crates/lib/tests/capabilities.rs` parks a block iterator's pipeline in the
  caller's `Read` (a stream whose reads off the building thread wait on a
  channel) and checks the values a caller holds at rest are accounted for.
  The word's saturation arithmetic is a unit test in `crates/lib/src/host.rs`, on the
  field helpers with forced boundary values, beside the seal's
  classification and a threaded race of the seal's exchange against
  retirements and nested mints.
- **Sealing.** `crates/lib/tests/seal.rs` (hook cfg, Unix) seals invocations that
  own a call sink on a real FIFO drained by `FifoCapture`, and reads the stream
  back. `FaultPlan::on_seal_step` parks or unwinds the finalizing thread at
  a `SealStep` (`Snapshot`, before the commit point; `Stream`, after the
  sealing emissions and before the terminal record), which is how
  concurrent sealers, a failed finalization and a failed stream are made
  deterministic; a tap parked inside an emission shows a seal refused while
  a callback is in flight. The sink's own endings (a terminal record a full
  FIFO turned away, a reader gone, an unwind before the terminal) are unit
  tests in `crates/lib/src/debug.rs`, on a FIFO whose reader never reads.
- **Publication hooks.** Publication is not cancellable, so no checkpoint
  lies inside it and the pauses above never land there. Instead it
  announces each namespace step that succeeded (`PublishStep`: a backup
  reserved, the previous output moved aside, a member installed or
  linked, a rollback step, a backup removed, the directory syncs done) to
  the token's publication observer, from the publishing thread, holding
  only the invocation's publication execution lock (so an observer must not
  publish under the same invocation: that is refused as re-entry; nor may it
  wait for another thread to publish under the same invocation, or for any
  work that needs that execution lock, since the lock is held until the
  observer returns and the wait never ends). The
  effect a step implies is recorded before the observer runs, so an
  observer that panics still leaves a truthful publication row. The
  publication log's own contract (ordinals, decisions, cancel outcomes,
  effects through every fault, re-entry, an unwinding observer) is tested
  through the public API in `crates/lib/tests/publication.rs`, parking at a
  `before_publish` hook or an `on_check` checkpoint and releasing over a
  channel within a bounded wait. The CLI, built with the hook cfg, reads
  `PBFHOGG_TEST_PUBLISH_FAULTS=<fault>,...` (faults spelled as
  `PublishFault` parses them; a bad spelling exits 101, as does a trace
  or announcement that cannot be written),
  `PBFHOGG_TEST_PUBLISH_STEPS=<file>` (appends each step as it happens)
  and `PBFHOGG_TEST_PUBLISH_PAUSE=<step>` (announces `published <step>` on
  `PBFHOGG_TEST_ANNOUNCE_FD` after that step and parks for good, so only a
  kill ends the run). The
  `killed_in_publication` tests in `crates/cli/tests/cli_output_contract.rs`
  record each scenario's steps with a counting run under the same faults,
  SIGKILL a run after every one, check the leftovers against the recorded
  steps replayed through a per-output model, remove `--staging-dir` and
  `--tmp-dir` whole, and run again; the library test
  `every_publication_announces_its_namespace_steps_in_order` pins the
  traces. A kill inside a backup's recursive removal is between two steps
  and is covered by confinement (it only deletes inside the backup), not
  sampled. `Staging::DirWithBackups` has no CLI spelling, so its kill
  states are covered in the library instead:
  `backups_in_the_staging_directory_leave_nothing_beside_a_destination`
  lists every destination's directory at every announced step of each
  replacing shape and failure path and the backups the staging directory
  then holds (empty when reserved and once restored, holding the previous
  output once moved), then checks what each destination and kept backup
  holds at the end. It walks the step prefixes a kill would leave, without
  killing anything.
- **CliInvoker for CLI-driven tests.** `crates/cli/tests/invoker.rs`. Every
  new `crates/cli/tests/cli_*.rs` goes through it. Every child times out at
  `TEST_CHILD_TIMEOUT`, set below `brokkr check`'s 20 s watchdog, with no
  per-test override. The timeout is one deadline from spawn shared by
  every wait on that child: `wait_announcement` (a pause point's typed
  announcement, read from a pipe the child holds the only writer of and
  waited on together with the child's pidfd, so it ends the moment the
  line is written; it reports the announcement, an exit first, or the
  deadline, never reaps, and fails on any other line) and the final
  `wait`, so a paused child is one bound, not two.
  Every child a test thread spawns through `common::process::OwnedChild`
  (CLI, harness and osmium children alike) also shares
  `TEST_CHILD_BUDGET`, anchored at the thread's first child: once it is
  spent the next spawn is refused and a wait that has not yet seen its
  child exit kills it, each naming the budget, so a test running children
  in sequence fails named instead of at the watchdog. The anchor is the
  first child, not test start, so it names the failure only when setup
  before that child stays inside the margin the tunables assert. A test
  whose children span more than the budget fails even if it would have
  ended under the watchdog: the budget is part of what a test must fit.
  A signal to
  a child the owner has already reaped is refused. Construction borrows `&TestScratch`, and the running
  handle carries that borrow through child and capture cleanup. `run` uses
  the same owner as `spawn().wait()`. Pipe reads are nonblocking, capture
  errors fail explicitly, and cleanup and EOF waits are bounded, including
  when another writer holds a pipe open. Exit waits block on the child's
  pidfd, opened at spawn, and a capture thread blocks on its pipe and its
  own stop eventfd, which only cleanup fires; nothing polls on a clock,
  except the exit re-check on a kernel without `pidfd_open`
  (`TEST_CHILD_POLL`). Output keeps the first and last MiB.
  The default cwd and `TMPDIR` are the borrowed scratch and stdin is null,
  and the child is `SIGKILL`ed if the thread that spawned it ends.
  The environment is cleared and nothing of the runner's is copied back:
  no PATH, HOME, locale or TZ (the child is started by absolute path, and
  an empty locale is the C locale). Hook, profiler and brokkr variables
  enter only through explicit setters, so the child's environment is
  exactly what its test set; see "The process environment" below. One setter is
  applied by default: `resources(cores, memory)`, at 4 cores and 8 GiB like
  the library's test hosts, sets `PBFHOGG_TEST_RESOURCES=<cores>:<bytes or
  none>`, which a child built with the hook cfg resolves to a `host::Fixed` in place of
  `MachineProbe`, and `RAYON_NUM_THREADS=<cores>`, which bounds the CLI's
  ambient executor in any build. These are fixed sizing and routing inputs,
  not a thread cap (`-j` and concurrent stage threads can exceed them). A
  child built without the hook cfg (the `consumer`, `musl` and `release`
  sweeps; `musl-all` sets it) ignores the resource variable and still sizes
  its pools, routes altw's auto backend and sets `check --locations`' store
  budget from the runner's machine. A shipped build has no private override
  to take, and adding the cfg would stop those sweeps testing the shape they
  exist for, so those runs are process-integration coverage: no assertion
  compiled into them may depend on the machine (one pinning a routed backend
  or a sizing outcome is gated on the hook cfg). The expected results are
  machine-independent; the cost of reaching them is not, since pool widths
  follow the runner's cores. Hook setters are
  named for the hook (`cancel_pause`, `cancel_count`, `publish_faults`,
  `publish_steps`, `publish_pause`, `publish_ready`, `replace_input`,
  `replace_with`, `replace_after`, `force_pin_path_reopen`,
  `merge_changes_spill_first`, `replace_report`); `marker_fifo` explicitly
  opts into sidecar emission. Arguments accept native paths. Relative output,
  default staging and default command scratch resolve inside the scratch cwd;
  explicit output, staging and `--tmp-dir` paths pass through untouched,
  including invalid paths used by refusal tests. Platform-skip
  predicates `is_o_direct_unsupported`, `is_uring_unsupported` require
  exit code 3 and a typed setup error. `.bind_mounted(source, target)`
  runs the child in a private user and mount namespace with `target` a
  bind mount of `source`, set up between fork and exec so the test
  process itself is untouched; a host that refuses the namespace ends
  the child with `NAMESPACE_DENIED` plus the step (skip), any other setup
  failure with `NAMESPACE_BROKEN` plus the step (fail).
  Binary located by cargo: `env!("CARGO_BIN_EXE_pbfhogg")`, spelled only
  as the program of `CliInvoker::new`'s `Command`, nothing read at run time.
- **Skips.** A test that cannot run on this host (no O_DIRECT, no
  io_uring, no osmium in PATH, no unprivileged user namespaces) calls
  `common::skip(reason)` (`test_support::skip` in a unit test under
  `crates/lib/src/`) and returns, or names the portion it drops when the rest of
  the test still runs.
- **Adversarial fixtures.** `crates/lib/tests/common/adversarial.rs` provides
  byte-level mutation primitives: `locate_blobs`,
  `mutate_blob_header_indexdata`, `mutate_blob_payload`,
  `truncate_to`. Hand-rolled varint reader, no internal pbfhogg
  imports.
- **Negative-id generators.** `crates/lib/tests/common/mod.rs` provides
  `generate_nodes_with_negatives`, `generate_ways_with_negatives`,
  `generate_relations_with_negatives` for mixed-sign fixtures (per
  `decisions/0002-negative-ids-rejected-project-wide.md`).
- **Test scratch.** The dev-only workspace member `crates/test-scratch`
  (package `pbfhogg-test-scratch`) is the one owner of test scratch for
  every harness. It is a leaf: no features and no dependency on `pbfhogg`,
  so no sweep's feature set reaches it, and the library, CLI and harness
  name it as a path-only dev-dependency with no version, which `cargo
  publish` strips. `pbfhogg_test_scratch::scratch()` returns a
  `TestScratch`: a random directory under the workspace's gitignored
  `data/test-scratch/`. Integration tests also reach it as
  `common::test_scratch()` and library unit tests as
  `path_guard::tests::scratch()`, both re-exports. The location is fixed
  two levels above the crate's manifest and checked once per process to be
  a workspace manifest; it is not relocatable and has no environment
  override, because the test runner exports no scratch-root variable. `repository_root()` in the
  same crate is the accessor for tests that read the checkout (the CLI
  reference, `brokkr.toml`), and it creates nothing.
  `child(name)` accepts exactly one normal path component and creates
  nothing; containment is lexical, so existing symlinks can still escape.
  `cell()` creates a fresh numbered subdirectory with a checked per-instance
  counter and refuses an existing entry. `anonymous_file()` allocates inside
  the owned scratch. `path_state(path)` (also available as a free function
  for recursive listing helpers) uses `symlink_metadata`, returns absence
  only for `NotFound`, and preserves other errors.
  On Unix, `deny_write(path)` borrows the scratch and probes whether creating
  an entry in the directory fails. An unavailable result is announced with
  the shared skip helper; unexpected setup or probe errors panic. This
  proves nothing about removal, rename, or opening existing entries.
  Its guard restores the original mode through an open directory descriptor,
  including after rename and during unwinding. Restoration failure panics
  on a normal drop and is reported to stderr during unwinding.
  Dropping a `TestScratch` during unwinding keeps its tree; any other drop
  removes it and panics, naming the path, if removal fails (a failed
  removal may leave part of the tree). Unwinding is not a test verdict: a
  guard created inside a closure whose panic a passing test catches is kept
  too, and a property failing through `prop_assert!` loses its case's tree.
  The removal panic fails a test only when the guard drops on the test
  thread or on a worker whose failure the test propagates, so keep guards
  as scoped owners (never `static` or thread-local) and outside any closure
  whose panic the test expects. `keep()` retains a tree deliberately.
  Residue in `data/test-scratch/` grows on unwinding, `keep()`, cleanup
  failure and process termination. A test binary never sweeps (it cannot
  tell a live sibling's tree from residue) and the test runner clears
  nothing, so `scripts/test_tree.py` does it at the run boundary, as two
  `brokkr check` script checks in `brokkr.d/lints.toml`. Before the test
  phase it empties `data/test-scratch/` (never following symlinks, and
  restoring owner permissions on directories a failed permission test left
  read-only) and records a stat manifest of the checkout. After a green
  test phase it fails if the checkout changed or the scratch base is not
  empty: a passing test that retains a tree removes it itself. A failing
  run's kept trees therefore survive until the next check's pre-test stage.
  The script's header lists what the manifest excludes and what it cannot
  see: undone writes, writes through symlinks, `brokkr test` runs, and runs
  whose test phase failed. An edit made by hand during a check fails it.
  The tree check compares stat metadata, not `git status` or content
  hashes, on purpose: git status misses writes inside existing ignored
  directories, skip-worktree files and mode-only changes (and `.gitignore`
  has unanchored `target`, `data` and `proptest-regressions` patterns),
  while a content digest would read the planet-sized inputs in `data/`. A
  post-test allowlist with no baseline was rejected because it fails on any
  dirty development tree, and an age-based sweep inside the test binary
  because it would delete trees the process did not create. Any new writer
  that runs before the test phase must run before `clear-and-snapshot`, and
  a background-stage writer would overlap it.
  Scratch constructor spellings are sealed outside the leaf by
  `test-scratch-via-test-scratch-crate` in `brokkr.d/lints.toml`.
  Companion rules seal manifest-directory discovery to the leaf and named
  source-contract checks, checked-in PBF names out of executable code, fixed
  `data/` literals, and static fixture-name counter spellings (including
  `RUN`). Each rule explicitly covers library, CLI and harness `src/` and
  `tests/` plus `crates/lib/examples/`, with `must_scan` sentinels.
  No test starts a shell, an interpreter, cargo, rustc, brokkr or any other
  general tool: a check that needs one is a `[[script_check]]` or textlint
  in `brokkr.d/lints.toml`. The source-policy rule `host_independence`
  (`crates/source-policy/src/rules/host_independence.rs`) holds every
  member's `src/` and `tests/` to that: each `std::process::Command::new` must
  be one entry of its closed inventory (file, enclosing item path, exact
  program tokens, count) - the pbfhogg binary only in `CliInvoker::new`,
  the harness executable, the osmium oracle and the self re-executions - so
  no literal program name (`sh`, `python3`, `cat`) and no second pbfhogg
  constructor that skips the promise guard can appear; `CARGO_BIN_EXE_`
  names only those owners; libc process creation (`fork`, `exec*`,
  `posix_spawn*`, `popen`, `system`, `clone`, their `SYS_` numbers) only in
  the guard's filter and violation probes; and no string literal carries a
  `#!/` script header. With no way to start one, a copied host binary
  needs no rule of its own. The shared `common/process.rs` child owner
  takes a built `Command` and constructs none. A child-owner test that needs a blocking child
  re-executes its own test binary into a probe test, as `guard::tests` and
  `invoker::tests::gated_child_probe` do.
  Doc-comment fixture paths remain legal, as does the fixture generator.
  These are spelling-bound tripwires, not proof of ownership: renamed
  imports and writes by absolute path outside the repository stay unseen.
  Keep the guard alive for the fixture lifetime. Never use benchmark scratch
  or fixed shared names.
- **Scratch tracking.** `crates/lib/tests/common/mod.rs` exports `snapshot_dir`
  and `assert_scratch_unchanged` for before/after comparisons
  around error paths, plus `with_tracked_scratch_dir(scratch_root,
  expected_new_paths, f)` wrapper.
- **Property tests.** `crates/lib/tests/proptests.rs`, entirely in memory:
  three properties of 64 cases each, run through explicit `TestRunner`s on
  one spelled-out `Config` with a fixed ChaCha seed. Neither
  `Config::default` nor the `proptest!` macro is used, because both consult
  `PROPTEST_*` environment variables; no environment setting changes case
  count, shrinking or replay, and a test pins the resolved settings and the
  seed's reproducibility. Failure persistence is disabled, so property runs
  never write regression files into the checkout. Promote a failing case by
  hand to a named test.
- **FIFOs.** Integration tests create a FIFO only with
  `common::make_fifo` and drain one only with `FifoCapture`
  (`crates/lib/tests/common/fifo_capture.rs`, declared once in
  `crates/lib/tests/all.rs` and mounted by path in `crates/harness/tests/exe.rs`).
  Its reader holds both ends (so no EOF spin before a producer opens), blocks
  in `poll` on the FIFO and a `UnixStream` shutdown channel, and is woken and
  joined by `finish` or on drop, unwinding included. The cleanup assertion
  is compiled only in the separate `fifo_cleanup` test target. Producers must be done
  before `finish`. A test feeding a FIFO to a run under test uses
  `common::FifoFeeder`, whose thread blocks in its write open until the run
  opens the read end; teardown, after the synchronous run has returned,
  releases a feeder still blocked there by opening a reader itself, and the
  feeder then reports `Unfed` without writing. No clock is involved. Two
  textlint rules in `brokkr.d/lints.toml` refuse a local
  `mkfifo` or FIFO drain in the test trees; unit tests under `src/` keep
  their own FIFOs.
- **Policy proposal (not-yet-adopted).** Every new parallel pipeline
  should ship with three tests: a worker-panic test, a `-j N` vs
  `-j 2` parity test, and a scratch-leak test. Worth considering as
  a CI gate.

Library tests get fixed descriptor grants from their test hosts; the CLI raises its
descriptor limit in a child. The
`process_nofile` target owns its process because it needs an unraised limit.
It checks descriptor counting by a controlled delta (eight files opened
between two counts) rather than assuming inherited descriptor numbers leave
no holes, and each scenario skips through the announced-skip helper when the
host's counting, free slots or hard limit cannot support it, restoring the
original soft limit at the end. It also refuses an external join under six
injected descriptor counts (counted with slots left, counted past the limit,
uncounted, a failed count, `EMFILE` and `ENFILE` exhaustion) through a
`ProcessActions` provider on the host, so those refusal messages are tested
without procfs, a real shortage or a limit change. Under the hook cfg it
witnesses the descriptor gate's per-run charge: it lists the open
descriptors (less the listing's own), sets the soft limit to that count
plus `TEST_HEAVY_RUN_FDS` (skipping if an open descriptor lies at or above
it), and runs the largest external join a heavy grant admits, its stage-1
fleets and relation input held open together by the fault plan's fleet
rendezvous, which must end `Complete`. Every scenario runs under one
libtest test, since all of them open descriptors and some change the
limit.
The harness package tests real probes the same way without a target of its
own: `crates/harness/tests/nofile.rs` reinvokes the `all` test binary as a
bounded `OwnedChild` with a fixed soft and hard `RLIMIT_NOFILE` set before
exec, and that child checks descriptor counting against an independent
`fcntl` oracle, raising to the hard cap, the harness's `RaiseToHard` policy,
a descriptor held above the soft limit and exhaustion. The parent never
changes its own limits. It skips where the host's hard limit is below the
fixture's. It is the one harness test that constructs a child outside
`exe.rs`, because the limit must be set in the child before exec.
The split is deliberate: a deterministic descriptor grant (an exact
`host::FdPool` budget, as the library harness's descriptor scenarios use)
tests command admission and sizing independently of inherited limits and
ambient opens, while child-process contracts test behaviour under a real
`RLIMIT_NOFILE`. Neither stands in for the other.

The shared I/O transfer size `IO_BUF_BYTES` (`crates/lib/src/tunables.rs`) has
one owner, checked by `crates/source-policy/src/rules/io_sizes.rs`:
no maximal integer-only literal expression in `crates/lib/src/read/` or
`crates/lib/src/write/` may evaluate to it. It is lexical, not Rust const
evaluation: named aliases, casts and macro-built arithmetic are outside its
promise, and a larger expression such as a scan budget is evaluated whole.

The Rust-source audits (these two, the test-module and test-import audits
above, the sealed lifecycle shapes, counter casts, the library static
inventory and the library text rule) are one crate,
`crates/source-policy`. Each file is read and tokenized once, with the
rustc tokenizer (`ra-ap-rustc_lexer`); lexical rules work on those tokens,
and only the module graph (walked by the test-module audit, and by the
static inventory to read `#[path]`-mounted library modules) and the
library text rule's test-only exemptions also parse with syn, failing on a
file it cannot parse. The real
tree is audited by one unit test, in the `all` sweep alone, so it runs once
per check and not under brokkr's markdown-only shortcut; every rule carries
in-memory fixtures beside it.

The sealed lifecycle shapes (`crates/source-policy/src/rules/lifecycle.rs`)
compare whole bodies of the scratch probe constructor `open_probe_lock` and
the CLI's `finish_invocation`, require `probe_lock` to open through the
former and `run` to call the latter once outside every delimiter, and allow
each counter-hook name only once in `crates/cli/src/lib.rs`, inside
`finish_invocation`. They are fixed token shapes, not Rust control flow;
behaviour is carried by the probe constructor's missing-file test and the
finalization test over success, refusal and cancellation.

Scratch entropy has one owner, checked by
`crates/source-policy/src/rules/entropy.rs`: the literal `getrandom` identifier may appear only
inside `random_u128` in `crates/lib/src/scratch.rs`, once, in production and
test code alike. It is lexical: aliases imported elsewhere, other entropy APIs
and macro-generated identifiers need review. Process-ID acquisition is not
confined there, because output publication names its sibling entries with
the PID independently of scratch identity.

Two textlint rules in `brokkr.d/lints.toml` keep test shapes that broke
before from returning. `counter-modules-do-not-sleep` bans the `sleep`
identifier in `crates/lib/src/counters.rs` and
`crates/lib/src/write/metrics.rs`: counter tests assert exact inputs or
synchronise on an observed slow path. `scale-scenarios-do-not-read-whole-files`
bans `fs::read`, `fs::read_to_string`, `read_to_end` and `read_to_string` in
`crates/harness/src/scenarios/`: dataset-sized scenario observations stream
through the bounded helpers in `file_assertions.rs`. Both are spelling-bound
tripwires, not semantic proofs.

The documentation link checker (`scripts/check_doc_links.py`, `doc-links`)
computes heading anchors for GitHub and for the pinned VitePress renderer
over a bounded Markdown subset. Its self-test (`doc-links-fixtures`) reads
the committed corpus `scripts/doc_links_fixtures/anchors.json`, whose
expected IDs were hand-derived from the installed VitePress source and from
GitHub's published section-link rules, never produced by the checker;
`scripts/doc_links_fixtures/README.md` records that provenance, the
sidebar-coverage boundary and the syntax outside the corpus. On a VitePress
upgrade the corpus is re-derived from the new source, not from checker output.

The renderer itself is the oracle in the complete gate. `doc-site`
(`scripts/check_doc_site.py`, gate only) builds the site with the pinned
VitePress and reads the generated HTML: every internal href, sidebar and
pager links included, names a built page and an id on it; every site page's
rendered sidebar links to the page, which is VitePress's per-route sidebar
selection rather than mention anywhere in the config; and the heading IDs
`doc-links` computes for each built page equal the built IDs, in order. It
runs only where node, pnpm and the installed lockfile
(`pnpm install --frozen-lockfile`) are present; elsewhere it names what is
missing, says the cross-check did not run, and passes (the degrade shape in
"Platform-dependent tests"). Where it runs it verifies the
site's actual pages, not general Markdown: syntax the site does not use is
still covered only by the corpus, and the GitHub surface still rests on
GitHub's documented rules, since GitHub's renderer cannot be run locally.

Platform setup refusals use `ErrorKind::PlatformUnsupported { feature, source }`.
`PbfWriter` and the file reader and writer constructors return
`pbfhogg::Result`, so the refusal reaches the caller typed. Only the standard
`Write`/`Read`/`Seek` impls keep their `io::Result` signatures and carry the
error inside `io::Error`; conversion to the library error restores the typed
variant. Every `O_DIRECT`
open (reader, direct header walk, direct writer, io_uring writer) classifies its refusal through
one helper, and path context added above it must keep the carrier typed. The
CLI exits 3 for this variant (see the exit-code table in
`docs/cli/commands.md`) and keeps its feature label in stderr; CLI skip
predicates require both that code and label. Runtime I/O failures are never
setup refusals, and raw `EINVAL` is never a sufficient reason to skip.
Library tests use `common::direct_io_unavailable` (it requires the typed
direct-I/O variant) and `common::is_uring_unavailable`, never their own errno
or message matching; both enforce the gate's requirement on a match. The one
exception is a test whose subject is the classification itself: it matches
the typed variant on the exact feature label it may skip on, never errno or
message text, and skips through `common::backend_unavailable` so the
requirement still applies. `is_uring_unavailable`
accepts `direct-io` as well (the io_uring writer opens its output
`O_DIRECT`), so `roundtrip::uring_writer_refuses_non_regular_targets`
skips only on `io-uring`: a `direct-io` label on a non-regular target is
the misclassification it pins. The library's unit tests under `crates/lib/src/` cannot
reach `crates/lib/tests/common`; they use `test_support` (`crates/lib/src/test_support.rs`),
whose `refused(cell, err, accepted)` takes the cell's backend and the labels
that may stop it (`is_direct_io_refusal` is its direct-io form) and whose
`skip` is `crates/lib/tests/common/skip.rs` mounted by `#[path]`, so their skips
announce `Skipping:` like any other.

The default CLI timeout leaves diagnostics time before check's 20 s watchdog.
