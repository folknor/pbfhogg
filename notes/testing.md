# Testing

Live tracker for pbfhogg's test infrastructure and coverage. Cross-ref
`reference/performance.md` for perf baselines and TODO.md's "Important:
ignored tests" section for the runbook on tests that don't run by default.

See [`testing-audit.md`](testing-audit.md) for the 2026-04-24 import
surface audit that drove the reorg plan below.

## Status summary

- **Test fixture infrastructure:** landed (2026-04-22).
- **CliInvoker** for CLI-driven integration tests: landed (2026-04-24),
  `tests/common/cli.rs`, smoke test in `tests/fixture_helpers.rs`. Zero
  new dev-deps. Hardened (2026-04-25, T12) with a 60 s default
  wall-clock timeout and platform-skip predicates
  (`is_o_direct_unsupported`, `is_uring_unsupported`); `cli_sort.rs`
  retrofitted to the predicates as the precedent for future
  `cli_*.rs`.
- **Fault-injection harness:** complete across all 8 parallel pipelines.
  Caught one real deadlock (apply-changes drain) and one real scratch
  leak (derive_parallel outer temp files); both fixed along the way.
- **Fault-injection split:** landed (2026-04-25). Eight per-binary
  `tests/fault_*.rs` files, one per pipeline. Static `PANIC_AT_*`
  atomics are now per-process and race-free, so all seven previously
  `#[ignore]`d tests run unguarded under `brokkr check`. The uring
  writer test still skips on hosts where io_uring init fails (low
  `RLIMIT_MEMLOCK`, missing kernel feature) - that is an environment
  skip, not the static-atomic race the split solved.
- **CLI-decoupled test reorg:** plan below. Motivation: internal module
  rewrites (ALTW stages, geocode passes, apply-changes pipeline) should
  not break integration tests. Today 18 of 30 `tests/*.rs` import
  internal command entrypoints or nested submodules and would need
  edits under any such rewrite. Conversion in progress.
- **Validation tiering:** `cli_sort.rs` re-split by tier intent
  landed (2026-04-25): osmium check `#[ignore = "external"]` (escape
  hatch), `--direct-io` / `--io-uring` variants in `mod platform`,
  fast contracts at file root. The `cli_sort.rs` shape is now the
  template for new `cli_*.rs` files. Wholesale conversion of every
  old test into the default `brokkr check` sweep is the wrong
  scaling model. External cross-validation lives in `brokkr verify`,
  not the in-tree test suite.

## Reorg: CLI-decoupled integration tests

**Thesis.** Integration tests in `tests/*.rs` must only touch the
stable library allowlist (fixture builders, `BlobReader`,
`ElementReader`, `PbfWriter`, `Element`, `MemberId`) or drive the
`pbfhogg` binary via `CliInvoker`. Internal-module tests live inline
in `src/**/*.rs` `#[cfg(test)] mod tests`, where they die with the
module on rewrite - which is correct.

**Five test layers end-to-end:**

| Layer | Where | What it tests | Survives internal rewrites? |
|---|---|---|---|
| 1. Inline unit | `src/**/*.rs` `#[cfg(test)]` | Module internals, invariants on the code right next to it | Dies with the module (intentional) |
| 2. Stable-API integration | `tests/roundtrip.rs`, `read_paths.rs`, `edge_cases.rs`, etc. | Public library API contracts (`PbfWriter`, `BlobReader`, `ElementReader`, ...) | Yes - stable allowlist only |
| 3. CLI integration | `tests/cli_*.rs` | Command behavior: input PBF + flags → output PBF; internal modules invisible | Yes - drives binary |
| 4. Fault injection | `tests/fault_*.rs` (one test per binary) | Error paths, panic recovery, scratch-dir cleanup in parallel pipelines | Partially - per-instance hooks on stable configs survive; static-atomic hooks on internals don't (acceptable: these tests are intentionally architecture-tied) |
| 5. Cross-validation | `brokkr verify` | Output equivalence vs osmium/osmosis/osmconvert on real datasets | Yes - process-level |

**Validation tiers:**

`#[ignore]` is only a Cargo mechanism, not the tiering model. Some
ignored tests are slow real-dataset tests, some are serial-only fault
injection, and some are platform-gated. Brokkr should expose semantic
profiles over those mechanics.

| Tier | Command shape | Runs | When |
|---|---|---|
| 1. Edit loop | `brokkr check` | Fast inline unit tests, stable-API tests, and small-fixture CLI command-contract tests. No real datasets, no external tools, no long fault-injection tests. | Every edit |
| 2. Command slice | `brokkr check --command <name>` or equivalent | Expanded in-project tests for one command family: adversarial fixtures, `-j` parity, scratch cleanup, and command-specific fault injection. | While working on that command |
| 3. Full in-project | `brokkr check --full` or equivalent | All in-project correctness tests, including slow/serial fault injection and real-Denmark/geocode tests. | Before merge/release, after broad library changes |
| 4. Scale/perf | `brokkr bench`, `brokkr suite`, overnight jobs | Planet-scale safety, performance, sidecar analysis, README table refreshes. | Performance work, release evidence |

External reference-tool cross-validation runs through `brokkr verify`,
which is a separate workflow rather than a tier. See "External
cross-validation" below.

Until brokkr has first-class Tier 2/3 profiles, use `brokkr test <name>`
for targeted debugging and keep expensive tests out of Tier 1 with
`#[ignore]` plus a clear runbook.

**Stable allowlist** - imports from this set do not couple the test to
an internal module shape:

- `pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata}`
- `pbfhogg::writer::{PbfWriter, Compression}`
- `pbfhogg::{BlobDecode, BlobError, BlobReader, BlobType, Element, ElementReader, ErrorKind, HeaderOverrides, MemberId, MemberType}`

Everything else is non-stable and requires CLI conversion.

**Conversion priority** (by rewrite-coupling × test count; see the
audit doc for full reasoning):

1. `cli_apply_changes.rs` - absorbs `merge.rs` + `apply_changes_invariants.rs` + `cluster2_defensive_input.rs` + `derive_changes.rs`. 51 tests, highest-traffic rewrite surface.
2. `cli_diff.rs` + `cli_derive_changes.rs` - split for file size. 45 tests combined.
3. `cli_extract.rs` - landed (2026-04-25). 27 tests converted; 9
   non-stable symbols (Region, ExtractStrategy, PolygonRings,
   ExtractSlot, etc.) replaced by --bbox/--polygon/--config CLI
   surface.
4. `cli_add_locations_to_ways.rs` - landed (2026-04-25). 18 tests
   converted; the four index backends (dense / sparse / external /
   auto) addressed via `--index-type` CLI flag instead of the
   `IndexType` enum. ALTW rewrite (notes/altw-external.md) is now
   unblocked from a tests/ perspective.
5. `cli_sort.rs` (landed), `cli_time_filter.rs` (landed 2026-04-25),
   `cli_merge_changes.rs` (landed 2026-04-25), `cli_cat.rs`
   (landed 2026-04-25), `cli_tags_filter.rs` (landed 2026-04-25),
   `cli_getid.rs`, `cli_renumber.rs`, `cli_tags_count.rs` - 126
   tests across 11 existing files. Convert
   them after the tier split is clear; do not blindly mirror every
   old test in Tier 1. Note that text-output commands
   (`tags-count`, `getid`, `inspect --tags`) have a different test
   shape from PBF-output commands: their library-internal-invariant
   tests should move inline (`#[cfg(test)] mod tests` in `src/`)
   rather than being CLI-converted, with a separate small
   `cli_<command>.rs` for the CLI-contract surface.

**CLI conversion policy.** Continue the `CliInvoker` direction, but do
not convert old test files wholesale into always-on `cli_*.rs` files.
Each command should be split by test intent:

- **Tier 1 CLI contract tests**: small fixtures, deterministic, no
  external tools, no platform-specific features unless they are cheap
  and reliable on the reference host.
- **Tier 2 command-slice tests**: larger per-command matrices, `-j`
  parity, truncation/adversarial sweeps, scratch assertions, and
  command-specific fault injection.
- **Platform tests**: `--direct-io`, `--io-uring`, MEMLOCK-dependent
  paths, and feature-missing CLI behavior. Keep out of Tier 1 until
  binary/library feature parity is fixed and the host behavior is
  deterministic.
- **External comparisons**: tests that invoke `osmium`, `osmosis`, or
  `osmconvert` belong in `brokkr verify`, not the in-tree test suite.
  See "External cross-validation" below for the offload rationale and
  migration plan.

`tests/cli_sort.rs` has been re-split by tier intent (2026-04-25).
The fast sort-contract tests stay at file root (Tier 1). The osmium
cross-check carries `#[ignore = "external"]` as the documented
escape-hatch convention until it migrates to `brokkr verify`. The
two platform variants (`--direct-io`, `--io-uring`) live inside
`#[cfg(any(...))] mod platform { ... }` so the brokkr platform
profile (T11) can target them via `cargo test platform::`. New
`cli_*.rs` files for apply-changes, diff, extract, ALTW follow the
same shape.

**Known harness gap:** CLI binary feature parity across test sweeps
is a brokkr-side concern, not a pbfhogg one. See
[`testing-cli-feature-parity.md`](testing-cli-feature-parity.md)
(the brokkr feature-requests handoff doc, request 2) for the problem
statement + proposed fix. Blocks feature-missing error tests for
every CLI-gated flag (`--direct-io`, `--io-uring`) across all
commands. Until the fix lands, the recommended fallback is inline
unit tests in `src/commands/mod.rs` under
`#[cfg(all(test, not(feature = "...")))]`.

**Fault-injection split** - landed 2026-04-25. Each cargo integration
test file compiles to its own binary, so the `PANIC_AT_*` static
atomics are per-process and race-free without `#[ignore]` or
`--test-threads=1`.

- `tests/fault_injection.rs` deleted; six pipeline modules (uring,
  diff_parallel, derive_parallel, geocode_pass3, altw_stage3,
  parallel_gzip) lifted to per-binary files.
- Two apply-changes fault tests moved out of
  `tests/apply_changes_invariants.rs`: `fault_apply_changes.rs` houses
  the per-instance `MergeOptions::panic_at_blob_seq` test;
  `fault_parallel_writer.rs` houses the static-atomic
  `parallel_writer` test.
- Hook-consolidation rule below: explicitly don't consolidate -
  per-binary isolation relies on the atomics being distinct symbols
  in distinct binaries.

## External cross-validation: brokkr-side

External cross-validation (`pbfhogg sort` vs `osmium sort`,
`pbfhogg merge` vs `osmium apply-changes`, etc.) lives in `brokkr verify`,
not the in-tree test crate. The decision and rationale:

- The `VerifyHarness` template (`run_pbfhogg`, `run_tool`, `diff_pbfs`,
  `check_sorted`, dataset config, variant matrix, results storage) already
  exists. Each new comparison is `verify_<command>.rs`, ~50 lines, same
  shape every time. Growth is bounded.
- External tools (osmium, osmosis, osmconvert) are operationally a
  brokkr concern, not a pbfhogg test-crate dependency. Contributors do
  not need osmium installed to get a clean `brokkr check`.
- An in-tree `mod external` tier would duplicate brokkr's verify
  machinery in cargo profiles and act as a gravity well: every new
  in-tree osmium test invites the next contributor to add another
  in-tree osmium test rather than a `verify_*.rs` entry.

**Two existing in-tree tests are migration candidates:**

1. `tests/merge.rs::merge_cross_validate_osmium` - real Denmark data,
   same inputs as `brokkr verify merge`. Retire once we confirm
   `verify_merge.rs` handles the version-vs-unconditional-delete
   tolerance that `tests/merge.rs:1271-1295` does explicitly (osmium
   uses version-based deletes; pbfhogg/osmosis/osmconvert delete
   unconditionally, so osmium-only elements that fall in the OSC
   delete set are not real failures).
2. `tests/cli_sort.rs::sort_cross_validate_osmium` - handcrafted
   overlapping-blob fixture; `brokkr verify sort` runs against real
   data only and so does not exercise the streaming sweep merge's
   overlap-run path. Migration requires `brokkr verify <command>
   --input <path>` plus an `examples/overlapping_fixture.rs` (or
   equivalent) builder, then move the comparison into `verify_sort.rs`.
   Until that brokkr feature lands, the in-tree test stays as-is,
   guarded by the `osmium --version` skip it already has.

**Escape hatch.** If a contributor mid-PR wants to write an osmium
check next to a fixture for the duration of that PR, that is
`#[ignore = "external"]` in-tree with a runbook comment, then converted
to a `verify_*.rs` PR against brokkr afterward. This is a permitted
exception, not a tier - it should not survive across many PRs.

## Conventions

- **`test-hooks` Cargo feature.** Gates fault-injection hooks across every
  parallel pipeline. Off by default; enabled under `--all-features`
  (which `brokkr check` uses). Release builds never see the hook code.
- **Two hook shapes.** Per-instance field on a public config struct (used
  by apply-changes via `MergeOptions::panic_at_blob_seq`; race-free with
  sibling tests) vs. process-global static atomics (used by writer-pool
  and shard-parallel pipelines whose workers are spawned deep inside
  constructors). Picker: per-instance when the pipeline has a public
  config struct on its entry path, static atomics otherwise. Once the
  fault-injection split lands, static-atomic hooks don't need
  `#[ignore]` either - the per-binary isolation makes them race-free.
- **CliInvoker for CLI-driven tests.** `tests/common/cli.rs`. Every
  new `tests/cli_*.rs` goes through it. The binary is found via
  `CARGO_TARGET_DIR` (or `CARGO_MANIFEST_DIR/target`) + debug/release
  from `cfg!(debug_assertions)`. `brokkr check` and `brokkr test` both
  build the binary as part of the workspace test run, so it exists by
  the time a CLI test starts.
- **Scratch tracking.** `tests/common/mod.rs` exports `snapshot_dir` and
  `assert_scratch_unchanged` for before/after comparisons around error
  paths.
- **Hook consolidation (explicitly don't).** The static-atomic
  submodules across parallel_writer / parallel_gzip / uring_writer /
  diff-parallel / derive-parallel / altw-stage3 / geocode-pass3 are
  structurally identical (`PANIC_AT_*` + `*_COUNT` + `reset()`), but
  must stay per-module. The fault-injection split depends on each
  binary owning its own copy of the atomics; folding into a shared
  module would re-introduce the cross-test races the split solves.
- **Policy proposal (not-yet-adopted).** Every new parallel pipeline
  should ship with three tests: a worker-panic test, a `-j N` vs `-j 2`
  parity test, and a scratch-leak test. Bug density in the sweep skewed
  hard toward the three newest / biggest parallel subsystems, and T05 +
  T06 + T09 exist precisely because earlier pipelines didn't have this
  discipline from the start. Worth considering as a CI gate once the
  reorg lands.

## Open work

Work item IDs are fixed and stable. Cite by ID in commits / ADRs /
other notes.

**Reshape under the reorg:** T02 and T03 are still standalone
infrastructure items - they produce fixture primitives the cli_*.rs
tests consume. T04, T05, T06 become *patterns applied inside each
cli_*.rs file* rather than standalone integration tests: a
truncation sweep, a `-j N` parity matrix, and a
`with_tracked_scratch_dir` assertion are natural per-command
concerns, not separate test files. Their item text below still
describes the correct sites and shapes; the surface is just
cli_*.rs instead of tests/command_name.rs. T07, T08, T09, T10 are
unchanged by the reorg.

### T02 - Lying-indexdata fixture primitives (extended coverage)

Cluster 2 of the 0.3.0 sweep (ADR-0004) landed the runtime half:
five hard-error promotions + `tests/cluster2_defensive_input.rs` with
two seed regression tests. The byte-level fixture helper itself is
still missing.

**Shape:** `tests/common/adversarial.rs` with two primitives:

- `mutate_blob_header_indexdata(pbf_bytes, blob_idx, f)`
- `mutate_blob_payload(pbf_bytes, blob_idx, f)`

so individual tests can inject reversed / overshooting indexdata
ranges, truncated varints in relation memids, and DenseNodes with
adversarial granularity without hand-rolling wire-format manipulation
per test.

**Test backlog unlocked by the primitives:**
- Three cluster-2 fixes that lack direct regression tests:
  `scan_ids.rs` overflow, `wire_rewrite.rs::count_varints_strict`,
  `stage1.rs` reversed range.
- Additional indexdata-trust sites not covered by cluster 2:
  `renumber/pass1.rs:179`, `renumber/wire_rewrite.rs:272`,
  `renumber/stage2.rs:226-231`, `altw/external/stage4.rs:438-478`,
  `apply_changes/scanner.rs:162,188`, `apply_changes/streaming.rs:496`,
  `commands/inspect/show_element.rs:53-57`.

### T03 - Negative-ID / signed-arithmetic matrix

~8 findings mishandle negative element IDs because guards are gated on
indexdata or shard planners use raw numeric compare instead of
`osm_id_cmp`. Every current fixture uses non-negative IDs.

**Shape:** add `generate_nodes_with_negatives(start_neg, start_pos, n)`
plus way/relation equivalents to `tests/common/mod.rs`. Canonical OSM
order: `..., -3, -2, -1, 0, 1, 2, ...`. Run every command through the
mixed-sign fixture, including `-j N` variants.

The `renumber` deviation in DEVIATIONS.md says "negative inputs
rejected" - we currently only test the happy path with indexdata
present.

**Sites covered:**
- `renumber/pass1.rs:179`, `renumber/wire_rewrite.rs:272,519-524`
- `diff/parallel.rs:138-142,354-357,384`
- `derive_parallel.rs:136-142` + sibling emit/merge sites
- `geocode_index/builder/pass1_5.rs:102`

Pairs with T05 (`-j N` parity) for maximum coverage - the
shard-parallel bugs only surface on mixed-sign inputs.

### T04 - Adversarial / truncated-input tests

~10 findings accept untrusted input without bounds-checking: missing
`MAX_BLOB_HEADER_SIZE` guards in the new pread primitives, schedule
offsets past EOF from truncated files, varint miscount on malformed
fields.

**Two shapes cover the class:**
1. The proptest baseline in T07 (parse-never-panics).
2. A "truncation sweep" integration test that takes a known-good PBF
   and truncates to every blob/frame/field boundary, asserting every
   command returns a clean `Err` without panic or multi-GB
   allocation.

**Sites covered:** `read/header_walker.rs:149-164`,
`read/raw_frame.rs:65-67,124-127`, `scan/classify.rs:59-95,110-163`,
`renumber/wire_rewrite.rs:486-491`, and the two geocode bucket-file
truncation findings.

### T05 - `-j N` vs `-j 1` parity matrix

Existing parity coverage: `inspect --nodes`, `tags-filter` two-pass,
`tags-count`, `merge_jobs_parity_on_multiblob_input`.

**Missing:**
- `diff -j N`
- `derive-changes -j N`
- `apply-changes -j N` (beyond the single merge fixture)
- altw external stage 4 worker count (currently hard-coded; would
  need a library arg)
- geocode Pass 1.5 / Pass 3 Stage A parallel degree
- `check --refs` - blocked on T09

Same shape as the existing tests: element-equivalent output + matching
summary counters across worker counts. Pins regression of the
diff/derive shard numeric-compare family, the `OwnedBytes` counter
bug, and any future worker-count-dependent drift. Pair with T03 for
maximum coverage.

### T06 - Scratch-dir / temp-file cleanup invariants

~8 findings leak scratch files on worker-error paths. Partially
covered today: every fault-injection test already uses
`snapshot_dir` / `assert_scratch_unchanged` to pin scratch cleanup
on its own error path.

**Remaining:** a generic `with_tracked_scratch_dir(|scratch| { run_command(...); })`
helper in `tests/common/mod.rs` for tests that aren't fault-injection
shaped but still want scratch-dir assertions. Combined with the
existing fault-injection coverage, catches every leak surfaced by
the sweep (altw external stages, diff-parallel, derive-parallel,
apply-changes `rewrite.rs:244` mid-stream-abort path, geocode Pass 3
Stage A).

### T07 - Property-based testing via `proptest`

Recommended first pass before any `cargo-fuzz` investment (T10). Same
class of bugs - parse crashes, boundary violations, roundtrip
asymmetries - but runs inside `cargo test` in seconds, no corpus
directory to gitignore, no long-running campaigns. Shrinks failing
inputs to minimal reproducers.

**Rough targets** (one `#[proptest]` fn each):

- `PrimitiveBlock::from_vec(bytes)` over arbitrary `Vec<u8>` - must
  return `Err` or `Ok`, never panic. Same shape for
  `parse_osc_file(bytes)`, `Cursor::parse_*`, `WireBlock::parse`,
  `WireInfo::parse`.
- `generate_nodes(n, start)` / `generate_ways` / etc → write → read →
  `assert_elements_equivalent` over arbitrary element counts and
  start IDs.
- `apply_changes(base, derive_changes(base, modified))`
  element-equivalent to `modified` over arbitrary-shape
  modifications to a baseline fixture (add/remove/modify N elements
  for arbitrary N).
- Header flag combinations: `sorted`, `bbox`, writing program,
  `required_features` → round-trip equality.

**Scope:** ~100-200 lines across one new `tests/proptests.rs`. Add
`proptest = "1"` to `[dev-dependencies]`. Runs in the normal
`brokkr check` sweep; no separate workflow.

### T08 - Boundary-twin scan across modules

Lowest-effort lever. Several findings are direct cross-module twins
of bugs already fixed:

- `commands/sort/mod.rs:178-181` is the same overlap-run kind-boundary
  bug as the just-fixed `cat/dedupe.rs:225`
- `write/parallel_writer.rs` and `write/parallel_gzip.rs` both
  silently swallow `Drop`-path errors
- The kind-placeholder-on-non-indexed pattern from apply-changes
  recurs in altw, extract-multi, getid, cat, tags-filter

**Practice:** when landing a fix in one module, add one regression
test per twin site in the same commit. Cheaper than chasing each
finding individually; prevents the next regression of the same
pattern.

### T09 - Parallel-classify parity test for `check --refs`

Other three parallel-classify commands (`inspect --nodes`,
`tags-filter` two-pass, `tags-count`) got `jobs=1` vs `jobs=4` parity
tests via their `jobs: Option<usize>` / `jobs: usize` library APIs.
`check_refs` has no equivalent override in its public signature
(`src/commands/check/refs.rs:141`), so a parity test has to either
exercise the CLI via `cli/tests/cli.rs` (hard to observe worker count
from outside) or wait for a plumbed `jobs` argument.

Not urgent - worker-count-independent correctness is implicitly
covered by existing single-blob tests. Revisit if `check --refs` ever
grows a `jobs` flag. Unblocks the final entry in T05.

### T10 - Fuzz testing via `cargo-fuzz`

Optional follow-up to T07; only worth the setup if someone wants to
run weekend campaigns. PBF parsing (`PrimitiveBlock::from_vec`), OSC
parsing (`parse_osc_file`), and wire-format decoders (`Cursor`,
`WireBlock`, `WireInfo`) all accept untrusted input. Targets for these
entry points would catch panics, OOM, and logic errors on malformed
data. Also fuzz the roundtrip path (write → read → compare).

**Cost:** `fuzz/corpus/` grows to hundreds of MB - low GB per target
over long campaigns, and `fuzz/target/` is ~500 MB - 1 GB of build
artifacts. Both must be gitignored; a developer running the fuzzer
locally needs that space.

**Schedule:** smoke runs (60 s) only verify the harness; real
bug-hunting needs hours to days per target ("weekend campaign"
cadence). Skip until T07 exposes a gap that only coverage-guided
fuzzing can fill.

### T11 - Brokkr validation profiles

`brokkr check` is becoming too broad as the in-project test suite
grows. Add explicit profiles that map to the validation tiers above
instead of relying on Cargo's raw `#[ignore]` switch:

- Tier 1 stays `brokkr check` and must keep a hard developer-loop wall
  budget.
- Tier 2 should run one command family by name, so command rewrites can
  ask for the relevant expanded suite without paying for every other
  command.
- Tier 3 should replace "run all ignored tests" with a named
  full-in-project gate that includes slow/serial tests intentionally.

This is brokkr work, not pbfhogg library work. Until it lands, new
slow, platform-specific, or real-dataset tests should not be added to
the default sweep just because they are integration tests. New
external-tool comparisons should not be added to the in-tree suite
at all - see "External cross-validation" above.

#### Proposed brokkr-facing model

Use normal Rust test module paths as the annotation surface. Do not add
pbfhogg-specific custom attributes, and do not treat `#[ignore]` as the
tier label. `#[ignore]` should remain a libtest execution mechanic for
tests that must never run accidentally, such as serial fault-injection
or platform-only cases.

Tier 1 tests may live at the integration-test root or in a `tier1`
module. Non-default tests should carry one of these module-path markers:

```rust
#[test]
fn sort_basic_cli_contract() {}

mod tier2 {
    #[test]
    fn sort_many_blob_boundaries() {}
}

mod tier3 {
    #[test]
    fn sort_large_fixture_roundtrip() {}
}

mod platform {
    #[test]
    fn sort_direct_io_alignment() {}
}

mod serial {
    #[test]
    #[ignore = "run through brokkr profile serial/fault"]
    fn injected_write_failure_is_atomic() {}
}
```

For command-family CLI tests, keep the current `tests/cli_<command>.rs`
shape and split intent inside the file:

- Root or `tier1` module: small CLI contracts that belong in
  `brokkr check`.
- `tier2`: expanded in-project command matrices, adversarial fixtures,
  `-j` parity, scratch cleanup, and command-local fault injection.
- `tier3`: slow but self-contained correctness checks, including larger
  in-repo or configured local fixtures.
- `platform`: direct I/O, io_uring, MEMLOCK, filesystem, and
  feature-surface checks.
- `serial`: tests that require `--test-threads=1` or static
  fault-injection state.

The brokkr implementation should translate named profiles into ordinary
Cargo/libtest arguments: `--test cli_sort`, substring filters,
`--skip`, `--include-ignored`, `--test-threads=1`, feature sweeps,
environment variables, prerequisite-tool checks, and explicit CLI
binary builds. That keeps the model transparent to other Rust projects
instead of baking pbfhogg internals into brokkr.

#### Proposed `brokkr.toml` shape

Do not add these keys to the live config until brokkr supports them.
This is the intended target shape:

```toml
[test]
default_package = "pbfhogg"
default_profile = "tier1"

[test.sweeps.all]
features = "all"
build_packages = ["pbfhogg-cli"]

[test.sweeps.consumer]
no_default_features = true
features = ["commands"]
build_packages = ["pbfhogg-cli"]

[test.profiles.tier1]
description = "Fast edit loop used by brokkr check"
sweeps = ["all", "consumer"]
skip = ["tier2::", "tier3::", "platform::", "serial::"]
include_ignored = false

[test.profiles.sort]
description = "Expanded sort command tests"
extends = "tier1"
tests = ["cli_sort"]
skip = ["platform::", "serial::"]

[test.profiles.tier2]
description = "All expanded in-project command tests"
sweeps = ["all", "consumer"]
only = ["tier2::"]
include_ignored = false

[test.profiles.full]
description = "All in-project correctness tests"
sweeps = ["all"]
skip = ["platform::"]
include_ignored = true

[test.profiles.platform]
description = "Platform-sensitive tests"
sweeps = ["all"]
only = ["platform::"]
include_ignored = true
env = { BROKKR_TEST_PLATFORM = "1" }

[test.profiles.serial]
description = "Serial/fault-injection tests"
sweeps = ["all"]
only = ["serial::"]
include_ignored = true
test_threads = 1
```

The command surface should stay generic:

```text
brokkr check
brokkr check --profile sort
brokkr check --profile tier2
brokkr check --profile full
brokkr verify
```

`brokkr check` uses `test.default_profile`. Command slices can be
implemented as profiles (`sort`, `extract`, `add-locations-to-ways`) or
as `--command <name>` sugar that resolves to a profile. The underlying
mechanism should remain profile selection so non-pbfhogg projects can
name their own slices.

### T12 - CliInvoker robustness before wider conversion

Landed 2026-04-25. `tests/common/cli.rs` now provides:

- Wall-clock timeout (default 60 s, override via `.timeout(Duration)`).
  Hung commands fail their test with a clear "timed out" panic instead
  of wedging `brokkr check`. Implemented via background drainer threads
  on stdout/stderr plus a polling `try_wait` loop that `kill()`s the
  child on expiry.
- `CliOutput::is_o_direct_unsupported` / `is_uring_unsupported`
  predicates that match the CLI's actual error strings (`Invalid
  argument` / `EINVAL` for O_DIRECT; `RLIMIT_MEMLOCK` / `kernel does
  not support` / `not supported` for io_uring). `tests/cli_sort.rs`
  switched to these helpers as the precedent for new `cli_*.rs` files.

Still open: feature-surface assertions stay out of CLI tests until
the feature-parity issue documented in
[`testing-cli-feature-parity.md`](testing-cli-feature-parity.md)
(brokkr-side, request 2) is fixed - or covered by inline unit tests
on the library-side CLI plumbing.
