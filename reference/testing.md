# Testing

Test placement, validation tiers, and conventions for pbfhogg.
Cross-ref `reference/performance.md` for perf baselines, and
"Tests that do not run everywhere" below for the runbook on tests
that skip or compile out by default.

## Test placement

Tests go where they couple least.

- **Inline unit tests** in `src/**/*.rs` `#[cfg(test)] mod tests` -
  module internals; die with the module on rewrite, which is the
  point.
- **Platform-dependent tests** go in a `platform` module so the platform
  profile selects them. Where the host lacks the feature (no O_DIRECT on
  the filesystem, io_uring refused) they skip and pass; nothing makes a
  feature mandatory.
- **Stable-API integration tests** in `tests/<topic>.rs` (e.g.
  `tests/roundtrip.rs`, `read_paths.rs`, `edge_cases.rs`) - use
  only the stable allowlist below.
- **CLI integration tests** in `cli/tests/cli_*.rs`, in the
  `pbfhogg-cli` package - drive the `pbfhogg` binary via `CliInvoker`;
  internal modules are invisible. Every test that spawns the binary
  lives there and nowhere else: cargo builds the binary for that
  package's integration tests with the test build's own features,
  profile and target, and hands its path as `CARGO_BIN_EXE_pbfhogg`.
  The root package's tests get no such path, so they have no way to
  reach the binary at all.
- **Fault-injection tests** in `tests/fault_*.rs`, modules of `all`
  like every other library test (plus a few unit tests beside the
  code they fault, such as `write::parallel_gzip`'s). Coupled to the
  `pbfhogg::test_hooks` surface by design.
- **External cross-validation** in `brokkr verify`
  (`verify_<command>.rs` modules in brokkr) - compares pbfhogg
  output against osmium / osmosis / osmconvert.

Placement and tiering are independent axes.

## Test binaries

Both crates set `autotests = false`; every integration-test target is
declared in its `Cargo.toml`. One binary per file made every check
sweep link and start one harness per file, and a focused `brokkr test
NAME` list each harness in turn to find one test.

- **`tests/all.rs`** (the library package) mounts every integration
  test file that can share a process as a module, and mounts
  `tests/common/` once. **`cli/tests/all.rs`** (the `pbfhogg-cli`
  package) is its twin for every CLI-driving test: the `cli_*.rs`
  files, `cli.rs`, and `invoker.rs` (the `CliInvoker`), with
  `tests/common/` mounted by `#[path]`. A test's full libtest name is
  `<file>::<path in file>` (`cli_sort::platform::...`) and carries no
  package, so tier filters (`tier2::`, `platform::`, ...) and leaf-name
  filters match in both binaries; a brokkr profile names the target
  `all` plus an `only` filter on the module (`cli_sort::`). Library
  test files gated on a feature carry the `#[cfg]` on their `mod` line
  in `tests/all.rs`, so it alone shows what drops out of the `minimal`
  sweep; the CLI binary has no such gates, since `pbfhogg-cli` always
  links the library with `commands`.
- **`tests/common/` is shared by path, not by a crate.** Each package
  compiles it with its own features, which carry the same names
  (`linux-direct-io`, `linux-io-uring`, `test-hooks`), so a `cfg` gate
  on those means the same thing in both. `commands` does not: in
  `pbfhogg` it is on by default, in `pbfhogg-cli` it is a no-op feature
  that is off in the `all` and `musl` sweeps. A
  `cfg(feature = "commands")` (or `geocode-reader`) gate in a shared
  helper would therefore silently drop CLI tests out of those sweeps, so
  the `no-library-feature-gates-in-tests-common` textlint rule in
  `brokkr.toml` bans both gates under `tests/common/`; gate the `mod`
  line in `tests/all.rs` instead. A helper crate would see only its
  own features, and its dependency on the library would have to keep
  `commands` off for the `minimal` sweep. `tests/common/` holds fixtures
  only; nothing in it spawns the binary.
- **A new test file not declared in its package's `tests/all.rs` (or as
  its own `[[test]]`) is never compiled, and no test or coverage audit
  reports it.** Add the `mod` line with the file.
- **A test that must own its process** gets its own `[[test]]` target
  in `Cargo.toml` with the reason written beside it; the `[[test]]`
  entries are the list. Arming a fault is not such a reason: fault
  state is per run (see "Conventions").
- **Moving tests between the packages: `scripts/test_inventory.py`.**
  It compares the tests a plan lists against the tests the tree now
  has (see its docstring for the exact brokkr command that produces the
  plan). It matches by test name only, so a lost test can be masked by a
  same-named test in the other package.
- **What the shared binary shares.** Every module of `all` runs
  concurrently in one process with: the global rayon pool; and
  `RLIMIT_NOFILE`, raised once per process by `common::test_host()`.
  Child processes inherit that limit. The sidecar metric sets (`WRITER_METRICS`,
  `PIPELINE_METRICS` and the worker pools' stall-counter sets among them) are process-global
  accumulators by design (the sidecar reads them as process totals), so
  concurrent tests contribute to the same totals and a delta of one
  can be moved, either way, by another test. A test that needs one
  run's exact count reads it from its token's fault plan instead:
  `FaultPlan::sync_calls` for the fsyncs a run's output finish and
  publication perform (`write::metrics::with_sync_observer`),
  `reorder_filled_high_water` for the read pipeline's reorder bound.
  A test that needs anything else here to itself takes a target of its
  own.
- **Process-wide panic hook.** The reader pipeline-death unit test temporarily
  swaps the panic hook to silence its deliberate worker panic. While the
  replacement is installed, it can also suppress panic output from another
  concurrently running unit test.
- **Fault hooks are per run, so fault tests share the harness.** No
  fault hook is process-global: their state lives in the fault plan
  carried by the run's cancellation token (see "Conventions"), so an
  armed fault reaches only the run handed that token, and a fault test
  runs concurrently with everything else in `all`. Two textlint rules
  in `brokkr.toml` keep it that way (`no-static-fault-hook-state`,
  `fault-hooks-only-in-test-hooks`).

The `pbfhogg-cli` program is its library target (`cli/src/lib.rs`,
`pbfhogg_cli`); the `pbfhogg` bin is only `main` calling
`pbfhogg_cli::run` and sets `test = false`. The library target exists
so the package's tests reach the command tree production parses,
`pbfhogg_cli::cli_command()`: the unit tests in `cli/src/` walk it, and
the command-table tests in `cli/tests/` take their subcommand lists
from it (`invoker::subcommands`, `invoker::subcommand_paths`), never
from `--help` text. It is not an API and sets `doctest = false`.

The `all`, `consumer` and `musl` check sweeps select both packages, so
the CLI tests run against a binary with `test-hooks` and both platform
features, one without the platform features (pbfhogg-cli's default
build), and one for musl. No sweep pre-builds the binary
(`build_packages`): cargo builds it for each test build. The `minimal`
and `geocode-reader` sweeps stay root-only: the CLI's lib dep would turn
`commands` back on. `brokkr test NAME` searches one package,
`[test] default_package = "pbfhogg"`; a CLI test needs
`brokkr test -p pbfhogg-cli NAME`.

## Validation tiers

Runtime-ranked. Each tier subsumes the cost of the previous; higher
tier = more expensive = run less often. `#[ignore]` is only a Cargo
mechanism, not the tiering model.

| Tier | Cost | When | Driven by |
|---|---|---|---|
| 1. Fast contracts | seconds | Every edit | `brokkr check` (default) |
| 2. Command slice | tens of seconds | While working on that command | `brokkr check --profile <cmd>` |
| 3. Full in-project | minutes | Before merge | `brokkr check --profile full` |
| 4. Scale/perf | hours | Performance work, release evidence | `brokkr bench`, `brokkr suite` |
| 5. External cross-validation | depends | Release gate | `brokkr verify` |

Tiers 1-3 are `brokkr check` profiles. Tiers 4-5 are separate
brokkr commands.

`brokkr check` enforces a fixed 20 s per-test watchdog with no
override, on every profile including `--profile full`. Tests slower
than that can never pass a `check` profile, so `brokkr.toml`'s
`[test.profiles.full]` carries a by-name `skip` list for the
over-watchdog tests - without it the profile could never pass, by
construction. Those tests are `#[ignore]`d out of `check`'s reach and
must be exercised individually via `brokkr test <name> --timeout
<secs>` (`brokkr test -p pbfhogg-cli <name> ...` for a CLI test), which raises the per-test timeout up to 280 s.

**No test reads a downloaded dataset.** Every test builds its own
input in scratch. Real-data coverage (Denmark, planet, replication
diffs) is `brokkr verify` and `brokkr bench`, which resolve datasets
per host from `brokkr.toml`; the test suite must pass on a checkout
with an empty `data/`.

**No test depends on the host.** No goldens blessed on one machine, no
core-count, memory or rlimit preconditions. `common::test_host()` and,
for unit tests under `src/`, `Host::for_tests()` hand the library a
`host::Fixed` (a fixed core count and memory budget), so library
tests see the same machine everywhere; a test that needs a
different machine sets its own `host::Fixed`. Tests may still skip where
the host lacks a feature (O_DIRECT, io_uring, osmium in PATH).

**Small configurations come through existing seams, not tunable
overrides.** The constants in `src/tunables.rs` get no runtime or
test-only override by default. A test that needs a small configuration
uses the host's resources and `-j` for worker counts, the engine's own
parameters (the ordered pool's `PoolConfig`, the parallel reader's
private budget and batch sizes) or a crate-private dispatch function; a
constant that blocks a named test gets a narrow private parameter at the
mechanism that owns it. A cross-crate test instrument (getparents'
full-scan threshold) is hidden and unsupported. The module doc of
`src/tunables.rs` gives the reasons.

**No checked-in input data.** Fixture PBFs are built at test time into
scratch by the `tests/common` writers (`write_test_pbf*`,
`write_multi_block_test_pbf`, `write_base_pbf` and its `changed` and
`bad_indexdata` variants) from the `generate_*` element generators, with
`common::adversarial` for byte-level damage. One exception:
`tests/test.osm.pbf` is the input the library's doc examples and guide pages
open by name, so an example a reader copies stays short and real, and a few
unit tests in `src/read/` read it too. It is small, host-independent and
regenerated by `examples/gen_test_pbf.rs`; nothing else may be checked in,
and new tests build their own input.

`mod platform` and `mod serial` are orthogonal config overlays, not
tiers. Same for `#[ignore = "external"]` (escape hatch for in-tree
osmium checks).

### Development contract

- **During refactor:** edit code + inline tests + any fault test
  whose hook moved. Run `brokkr check`. Green ⇒ refactor
  structurally landing. Tiers 2-5 stay silent during iteration.
- **Before merge:** run tiers 2-3 to confirm CLI behaviour through
  the stable surface.
- **Before release:** run tier 4 for performance regression and
  tier 5 for external parity.

Tier 1 must be both fast AND structurally complete: every internal
contract a refactor could break needs a tier-1 test (inline unit OR
fault hook).

Tiers 2-5 are by construction internal-refactor-immune: every test
goes through the stable allowlist or the CLI surface, so a rewrite
of `src/commands/<X>/` cannot break them by type changes alone.
That is the load-bearing property of the test reorg.

### Stable allowlist

Imports from this set do not couple a test to an internal module
shape:

- `pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata}`
- `pbfhogg::writer::{PbfWriter, Compression}`
- `pbfhogg::{Blob, BlobDecode, BlobError, BlobReader, BlobType, Element, ElementReader, ErrorKind, HeaderBlock, HeaderOverrides, MemberId, MemberType, Way}` (including `Blob::way_members`, `Blob::way_member_count`, `BlobReader::set_parse_waymembers`, `Way::shared_node_pins`, and `HeaderBlock`'s injected-prepass feature accessors)

Everything else is non-stable and requires CLI conversion.

### Per-command CLI test split

Each `cli/tests/cli_<command>.rs` splits intent inside the file:

- **Root or `tier1` module**: small CLI contracts; runs in
  `brokkr check`.
- **`tier2`**: expanded matrices, `-j` parity, adversarial
  fixtures, scratch cleanup, command-local fault injection.
- **`tier3`**: slow self-contained correctness, larger fixtures.
- **`platform`**: `--direct-io`, `--io-uring`, MEMLOCK, feature
  surface - keep out of tier 1 unless cheap and reliable on the
  reference host.
- **`serial`**: requires `--test-threads=1`. Fault injection is not a
  `serial` case: its state is per run.

`cli/tests/cli_sort.rs` is the canonical template for new `cli_*.rs`
files.

### Tests that do not run everywhere

- `sorted_flag_but_unsorted_nodes_panics` (`tests/read_paths.rs`)
  checks that the debug monotonicity assertion fires on unsorted
  nodes under a declared `Sort.Type_then_ID`. It is gated on
  `cfg(debug_assertions)`, not `#[ignore]`, so a build without debug
  assertions compiles it out and it cannot be run there at all; its
  doc comment says why the gate replaced `#[ignore]`.
- The io_uring tests (`roundtrip_uring_*` in `tests/roundtrip.rs`,
  `tests/fault_uring_writer.rs`) skip and pass when io_uring setup
  fails for an environmental reason, most often an `RLIMIT_MEMLOCK`
  soft limit too low for the writer's registered buffers. To run
  them on a development host, raise the limit system-wide in
  `/etc/security/limits.conf` (`@<your-group> - memlock unlimited`)
  and log in again.
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

**In-tree osmium checks are migration candidates.** Each is
`#[ignore = "external"]`, skips where osmium is absent, and exists
because its handcrafted fixture reaches a path a dataset run does not:

- `cli/tests/cli_sort.rs::sort_cross_validate_osmium` - overlapping-blob
  fixture; `brokkr verify sort` on a configured dataset runs against
  real data only and doesn't exercise the streaming sweep merge's
  overlap-run path. `brokkr verify <command> --input <path>` already
  accepts a handcrafted PBF; the missing piece is an overlapping-fixture
  builder under `examples/`, after which the comparison moves to
  `brokkr verify sort --input`.
- `cli/tests/cli_digest.rs::osmium_reencodings_digest_equal` - pins the
  encodings a real dataset run does not vary.
- `cli/tests/cli_apply_changes_osmium.rs` - `apply-changes
  --locations-on-ways` against osmium on a base and change file built so
  that both tools' documented semantics coincide (osmium's location
  index has ordering defects that decide its output otherwise; the
  module doc lists the fixture rules). `brokkr verify` has no
  `--locations-on-ways` path and takes OSC files only from the dataset
  configuration, so it cannot run this fixture yet.

**Escape hatch.** A contributor mid-PR can write an osmium check
next to a fixture for the duration of that PR as
`#[ignore = "external"]` in-tree with a runbook comment, then
convert to a `verify_*.rs` PR against brokkr afterward. Permitted
exception, not a tier - should not survive across many PRs.

## Conventions

- **`test-hooks` Cargo feature.** Off by default and never on in a
  product build, which therefore never sees the hook code; the `all`
  check sweep enables it.
- **Fault plans ride the token.** `pbfhogg::test_hooks` is the one
  answer to "what can a test inject". Its `FaultPlan` holds every
  token-scoped hook (arming fields and observation counters, public
  atomics, disarmed as a `MAX` sentinel); one lives in each
  `host::Invocation` and `Invocation::faults()` reaches it. The token is what
  an invocation already hands every worker loop it starts (the read
  pipeline, the output writers, the diff and derive shards, altw's
  stages, the geocode passes), so a new hook is a `FaultPlan` field
  read at the site through the token in hand, gated on
  `cfg(feature = "test-hooks")`. A test arms the plan of the token in
  the `Host` it passes; `common::test_host()` mints a fresh token per
  call. One token is one fault scenario: everything run under it shares
  its plan (sequence numbers, worker indices, counters), so an armed and
  an unarmed run use two tokens. A reader takes its token through
  `ElementReader::cancel`; a public path constructor mints its own,
  reached through `PbfWriter::test_faults`. Per-token isolation does not
  stop an armed run's own parked workers: a test that parks them still
  releases and joins on every exit path. Per-object hooks stay with
  their owner and are listed in the module docs: `PublishFault` on an
  output, the geocode reader's counter, and the cancellation hooks
  below.
- **Cancellation hooks.** `Invocation::on_check` (per-instance, `test-hooks`)
  runs a callback with each checkpoint's 1-based ordinal and may block;
  `Invocation::checks_passed` counts them. Library tests trip the token from
  the callback (`tests/cancel.rs`). The CLI, built with `test-hooks`, reads
  two variables: `PBFHOGG_TEST_CANCEL_COUNT=<file>` writes the run's
  checkpoint count there at exit, and
  `PBFHOGG_TEST_CANCEL_PAUSE=<k>:<ready file>[:stuck]` parks the thread
  reaching checkpoint `k`, writes the ready file, and waits for the token;
  `stuck` writes `<ready file>.ack` once tripped and never returns, so only
  a second signal ends the run. The interruption tests in
  `cli/tests/cli_output_contract.rs` count a command's checkpoints, park it at
  sampled ordinals (tier 1) or at every ordinal of the counting run (its
  `tier3` shards, run by `--profile full` and the gate), signal it, and
  assert the exit code and an empty directory, with no sleeps. Ordinals are
  positions, not sites, and the fixture's counts are not real data's, so
  neither sweep proves every checkpoint site of a planet run unwinds. `CliInvoker::spawn` returns a `RunningCli`
  that can be signalled and is killed on drop.
- **CliInvoker for CLI-driven tests.** `cli/tests/invoker.rs`. Every
  new `cli/tests/cli_*.rs` goes through it. Default wall-clock timeout
  set below `brokkr check`'s 20 s watchdog, override via
  `.timeout(Duration)`. Platform-skip
  predicates `is_o_direct_unsupported`, `is_uring_unsupported` require
  exit code 3 and a typed setup error.
  Binary located by cargo: `env!("CARGO_BIN_EXE_pbfhogg")`
  (`invoker::pbfhogg_bin`), nothing read at run time.
- **Skips.** A test that cannot run on this host (no O_DIRECT, no
  io_uring, no osmium in PATH) calls `common::skip(reason)` and returns.
- **Adversarial fixtures.** `tests/common/adversarial.rs` provides
  byte-level mutation primitives: `locate_blobs`,
  `mutate_blob_header_indexdata`, `mutate_blob_payload`,
  `truncate_to`. Hand-rolled varint reader, no internal pbfhogg
  imports.
- **Negative-id generators.** `tests/common/mod.rs` provides
  `generate_nodes_with_negatives`, `generate_ways_with_negatives`,
  `generate_relations_with_negatives` for mixed-sign fixtures (per
  `decisions/0002-negative-ids-rejected-project-wide.md`).
- **Test scratch.** `tests/common/scratch.rs` owns random, RAII-cleaned
  directories under the workspace's `data/test-scratch/` (also when the
  CLI package mounts it, whose manifest directory is `cli/`). It is the
  one implementation for every harness: integration tests reach it as
  `common::test_scratch()`, and unit tests under `src/` as
  `path_guard::tests::scratch()`, which mounts the same file by `#[path]`
  so the library's unit tests never link the integration harness. The CLI
  crate's own unit tests use `test_scratch::scratch()` (`cli/src/test_scratch.rs`),
  which cannot mount the integration file but has the same body. Every
  test scratch site goes through one of these (enforced by the
  `unit-test-scratch-via-path-guard` textlint in `brokkr.toml`, which covers
  `src/`, `tests/`, `cli/tests/` and `cli/src/`). Keep the guard
  alive for the fixture lifetime (the `TempDir` drop is the cleanup, so no
  manual `remove_dir_all` is needed). Never use benchmark scratch or
  fixed shared names.
- **Scratch tracking.** `tests/common/mod.rs` exports `snapshot_dir`
  and `assert_scratch_unchanged` for before/after comparisons
  around error paths, plus `with_tracked_scratch_dir(scratch_root,
  expected_new_paths, f)` wrapper.
- **Property tests.** `tests/proptests.rs`, 64 cases per property.
  `proptest-regressions/` is gitignored to avoid committing
  case-specific reproducers.
- **Policy proposal (not-yet-adopted).** Every new parallel pipeline
  should ship with three tests: a worker-panic test, a `-j N` vs
  `-j 2` parity test, and a scratch-leak test. Worth considering as
  a CI gate.

`pbfhogg::host` and `pbfhogg::process` are stable test imports. Library tests
use `NoProcessActions`; the CLI applies its arena cap in a child. The
`process_nofile` target owns its process because it needs an unraised limit.

Platform setup refusals use `ErrorKind::PlatformUnsupported { feature, source }`.
`PbfWriter` and the file reader and writer constructors return
`pbfhogg::Result`, so the refusal reaches the caller typed. Only the standard
`Write`/`Read`/`Seek` impls keep their `io::Result` signatures and carry the
error inside `io::Error`; conversion to the library error restores the typed
variant. Every `O_DIRECT`
open (reader, direct header walk, direct writer, io_uring writer) classifies its refusal through
one helper, and path context added above it must keep the carrier typed. The
CLI exits 3 for this variant (see the exit-code table in
`reference/cli-reference.md`) and keeps its feature label in stderr; CLI skip
predicates require both that code and label. Runtime I/O failures are never
setup refusals, and raw `EINVAL` is never a sufficient reason to skip.
Library tests use `common::is_einval` (historical name; it requires the typed
direct-I/O variant) and `common::is_uring_unavailable`, never their own errno
or message matching.

The default CLI timeout leaves diagnostics time before check's 20 s watchdog.
