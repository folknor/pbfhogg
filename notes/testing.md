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
  new dev-deps.
- **Fault-injection harness:** complete across all 8 parallel pipelines.
  Caught one real deadlock (apply-changes drain) and one real scratch
  leak (derive_parallel outer temp files); both fixed along the way.
  Eight tests still `#[ignore]`d due to shared-state races - the reorg
  un-ignores them by splitting into per-binary files.
- **CLI-decoupled test reorg:** plan below. Motivation: internal module
  rewrites (ALTW stages, geocode passes, apply-changes pipeline) should
  not break integration tests. Today 18 of 30 `tests/*.rs` import
  internal command entrypoints or nested submodules and would need
  edits under any such rewrite. Conversion in progress.

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

**Invocation:**

| Command | Runs | When |
|---|---|---|
| `brokkr check` | Layers 1-4, excludes `#[ignore]`d | Every edit |
| `brokkr check -- --include-ignored` | Adds `roundtrip_real` + `geocode_index` + the nightly-regression `sorted_flag_but_unsorted_nodes_panics` | Before release |
| `brokkr test <name>` | Single test by substring | Debugging |
| `brokkr verify all` | Layer 5 across every CLI command | Before release |

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
3. `cli_extract.rs` - 27 tests, 9 non-stable symbols imported today.
4. `cli_altw.rs` - 18 tests. Blocks ALTW rewrite (the motivating example).
5. `cli_sort.rs`, `cli_cat.rs`, `cli_getid.rs`, `cli_tags_filter.rs`, `cli_merge_changes.rs`, `cli_renumber.rs`, `cli_tags_count.rs`, `cli_time_filter.rs` - 126 tests across 11 existing files, mechanical applications of the pattern once #1 is landed.

**Known harness gap:** CLI binary feature parity across test sweeps
is a brokkr-side concern, not a pbfhogg one. See
[`testing-cli-feature-parity.md`](testing-cli-feature-parity.md) for
the problem statement + proposed fix. Blocks feature-missing error
tests for every CLI-gated flag (`--direct-io`, `--io-uring`) across
all commands. Until the fix lands, the recommended fallback is
inline unit tests in `src/commands/mod.rs` under
`#[cfg(all(test, not(feature = "...")))]`.

**Fault-injection split** - un-ignores all 8 tests, independent of
the CLI conversion work:

- Split `tests/fault_injection.rs` (+ the `apply-changes` panic test
  currently in `apply_changes_invariants.rs`) into eight binaries:
  `fault_apply_changes.rs`, `fault_parallel_writer.rs`,
  `fault_parallel_gzip.rs`, `fault_uring_writer.rs`,
  `fault_diff_parallel.rs`, `fault_derive_parallel.rs`,
  `fault_altw_stage3.rs`, `fault_geocode_pass3.rs`. Each cargo
  integration test file compiles to its own binary, so the
  `PANIC_AT_*` static atomics are per-process and race-free without
  `#[ignore]` or `--test-threads=1`.
- Hook-consolidation note below becomes "explicitly don't consolidate"
  - per-binary isolation relies on the atomics being distinct symbols
  in distinct binaries.

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
