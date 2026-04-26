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
- **CLI-decoupled test reorg:** all 5 priority-list surfaces landed
  (2026-04-25). Motivation: internal module rewrites (ALTW stages,
  geocode passes, apply-changes pipeline) should not break
  integration tests. Started with 18 of 30 `tests/*.rs` files
  importing internal command entrypoints; the priority-list files
  are now driven by `CliInvoker` against the stable CLI surface.
  Remaining `tests/*.rs` (`roundtrip*`, `read_paths`, `edge_cases`,
  `corrupt_input`, `non_indexed_parity`, `getparents`, `inspect`,
  etc.) are out of priority-list scope - they're either stable-API
  tests (allowlist-only, refactor-immune by construction) or
  text-output commands where library-internal-invariant tests
  should move inline rather than become CLI tests.
- **Development contract:** tier 1 (`brokkr check`) is the inner-loop
  signal during refactor. Tiers 2-5 are by construction
  internal-refactor-immune. See "Validation tiers" below for the
  full contract.
- **Validation tiering:** `cli_sort.rs` re-split by tier intent
  landed (2026-04-25): osmium check `#[ignore = "external"]` (escape
  hatch), `--direct-io` / `--io-uring` variants in `mod platform`,
  fast contracts at file root. The `cli_sort.rs` shape is now the
  template for new `cli_*.rs` files. Wholesale conversion of every
  old test into the default `brokkr check` sweep is the wrong
  scaling model. External cross-validation lives in `brokkr verify`,
  not the in-tree test suite.
- **Coverage layer 2 landed 2026-04-26:** T02 byte-level adversarial
  fixture primitives (`tests/common/adversarial.rs`), T03 negative-ID
  generators + `cli_negative_id_invariants.rs` sweep, T04 truncation sweep
  (`cli_truncation_sweep.rs`), T05 parity for diff/derive/apply, T06
  `with_tracked_scratch_dir` helper, T07 proptest baseline
  (`tests/proptests.rs`, `proptest = "1"` dev-dep). T05's remaining
  surfaces (ALTW external stage 4, geocode Pass 1.5 / Pass 3 Stage A)
  and T09 (`check --refs` parity) explicitly deferred - all three
  need lib API changes to expose `jobs`. T08 stays as practice memo;
  T10 (cargo-fuzz) explicitly out of scope.

## Reorg: CLI-decoupled integration tests

**Thesis.** Integration tests in `tests/*.rs` must only touch the
stable library allowlist (fixture builders, `BlobReader`,
`ElementReader`, `PbfWriter`, `Element`, `MemberId`) or drive the
`pbfhogg` binary via `CliInvoker`. Internal-module tests live inline
in `src/**/*.rs` `#[cfg(test)] mod tests`, where they die with the
module on rewrite - which is correct.

**Test placement.** Tests go where they couple least. After the reorg
the natural placements are:

- Inline unit tests in `src/**/*.rs` `#[cfg(test)] mod tests` -
  module internals; die with the module on rewrite, which is the
  point.
- Stable-API integration tests in `tests/<topic>.rs` (e.g.
  `tests/roundtrip.rs`, `read_paths.rs`, `edge_cases.rs`) - use only
  the stable allowlist below.
- CLI integration tests in `tests/cli_*.rs` - drive the `pbfhogg`
  binary via `CliInvoker`; internal modules are invisible.
- Fault-injection tests in `tests/fault_*.rs` (one per binary) - own
  their `PANIC_AT_*` hooks per-process. Partially coupled to internal
  hook surfaces by design.
- External cross-validation in `brokkr verify` (`verify_<command>.rs`
  modules in brokkr) - compares pbfhogg output against osmium /
  osmosis / osmconvert.

Placement and tiering are independent axes. Placement says where a
test lives and what it imports; tiering says how often it runs.

**Validation tiers (runtime-ranked).** Each tier subsumes the cost of
the previous; higher tier = more expensive = run less often.
`#[ignore]` is only a Cargo mechanism, not the tiering model.

| Tier | Cost | When | Driven by |
|---|---|---|---|
| 1. Fast contracts | seconds | Every edit | `brokkr check` (default) |
| 2. Command slice | tens of seconds | While working on that command | `brokkr check --profile <cmd>` |
| 3. Full in-project | minutes | Before merge | `brokkr check --profile full` |
| 4. Scale/perf | hours | Performance work, release evidence | `brokkr bench`, `brokkr suite` |
| 5. External cross-validation | depends on osmium/osmosis/osmconvert + dataset size | Release gate, after semantic rewrites | `brokkr verify` |

Tiers 1-3 are `brokkr check` profiles. Tiers 4 and 5 are separate
brokkr commands; they sit in the tier list because they are the
remaining gates a release passes through, not because `brokkr check`
runs them.

`mod platform` and `mod serial` are orthogonal config overlays, not
tiers. A platform test can sit at tier 1 (fast contract) or tier 3
(full sweep) independently; the marker says "needs particular host
setup" or "needs `--test-threads=1`", which is a different axis from
runtime cost.

**Development contract.** Tier 1 is the inner-loop signal during a
refactor:

- **During refactor:** edit code + inline tests + any fault test whose
  hook moved. Run `brokkr check`. Green ⇒ the refactor is
  structurally landing. Tiers 2-5 stay silent during iteration.
- **Before merge:** run tiers 2-3 to confirm CLI behaviour through the
  stable surface.
- **Before release:** run tier 4 for performance regression and tier 5
  for external parity.

Tier 1 contains **both** internal-coupled tests (inline unit, fault)
**and** immune tests (stable-API, small-fixture CLI). Tiers 2-5 are by
construction internal-refactor-immune: every test goes through the
stable allowlist or the CLI surface, so a rewrite of
`src/commands/<X>/` cannot break them by type changes alone. That is
the load-bearing property of this whole reorg.

Two design constraints fall out of the contract:

- **Tier 1 must be both fast AND structurally complete.** Fast because
  it is the inner loop; structurally complete because if `brokkr check`
  passes and the refactor is actually broken, the dev gets misled.
  Every internal contract that the refactor could break needs a tier-1
  test (inline unit OR fault hook) that catches it.
- **Tier 1 must not carry real-dataset cost.** A 54 s Denmark roundtrip
  belongs in tier 3, not tier 1, because waiting 54 s per iteration
  kills the inner loop. Tier 1 covers structural correctness on small
  synthetic fixtures; output correctness on real data lives in tier 3+.

**Stable allowlist** - imports from this set do not couple the test to
an internal module shape:

- `pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata}`
- `pbfhogg::writer::{PbfWriter, Compression}`
- `pbfhogg::{BlobDecode, BlobError, BlobReader, BlobType, Element, ElementReader, ErrorKind, HeaderOverrides, MemberId, MemberType}`

Everything else is non-stable and requires CLI conversion.

**Conversion priority** (by rewrite-coupling × test count; see the
audit doc for full reasoning):

1. `cli_apply_changes.rs` (and split siblings) - landed 2026-04-25,
   four stages across four files since the priority-1 source was
   spread across four originals (51 tests, highest-traffic rewrite
   surface).
   - Stage 1: `cli_apply_changes.rs` (21 tests from `merge.rs`).
   - Stage 2: `cli_apply_changes_invariants.rs` (11 tests from
     `apply_changes_invariants.rs`).
   - Stage 3: `cli_defensive_input.rs` (2 tests from
     `cluster2_defensive_input.rs`, multi-command).
   - Stage 4: `cli_derive_changes.rs` (15 tests from
     `derive_changes.rs`, derive→apply roundtrip via
     `pbfhogg diff --format osc` + `pbfhogg apply-changes`).
2. `cli_diff.rs` (landed 2026-04-25) + `cli_derive_changes.rs`
   (landed under priority 1 stage 4). 45 tests combined: 30
   text-format diff tests in `cli_diff.rs`, 15 derive-changes /
   roundtrip tests in `cli_derive_changes.rs`.
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

**Harness gap (resolved 2026-04-25):** CLI binary feature parity across
test sweeps was previously a brokkr-side concern. Three brokkr commits
landed it the same day: `f7a96b7` introduced `build_packages` on
`[[check]]` entries, `2235792` collapsed sweeps into the `[[check]]`
array as the single primitive, and `b3aa444` exports
`BROKKR_TEST_BIN_DIR=<target>/<profile>` per sweep so test code
doesn't have to guess the profile via `cfg!(debug_assertions)`. Pbfhogg's
`brokkr.toml` consumer sweep now sets `build_packages = ["pbfhogg-cli"]`
so the CLI binary is rebuilt without linux features for that sweep, and
the `feature_missing_error` tests in `cli_sort.rs` (gated on
`cfg(not(feature = "linux-..."))`) fire correctly via the env-var
binary lookup. Caveat: `pbfhogg-cli` carries a no-op `commands = []`
feature in `cli/Cargo.toml` so brokkr can apply `--features commands`
symmetrically to both crates (the CLI's lib dep already always pulls
in `commands` - the feature is purely a brokkr-symmetry concession).

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

### T02 - Lying-indexdata fixture primitives [LANDED 2026-04-26]

`tests/common/adversarial.rs` ships three primitives:

- `locate_blobs(pbf) -> Vec<BlobLocation>` - byte ranges per frame
- `mutate_blob_header_indexdata(pbf, idx, f)` - rewrites the
  BlobHeader.indexdata bytes in-place
- `mutate_blob_payload(pbf, idx, f)` - decompresses the blob,
  hands the inner PrimitiveBlock bytes to the caller, re-emits as
  a raw (uncompressed) Blob with recomputed datasize / frame
  length prefix
- `truncate_to(pbf, len)` - shared with T04

The helper is self-contained (hand-rolled varint reader, no pbfhogg
internal imports) so internal-module rewrites cannot break it.

`tests/cli_defensive_input.rs` adds three regression tests built on
the primitives:

- `altw_external_rejects_reversed_indexdata_range` - swaps the
  min_id/max_id indexdata bytes; pins `stage1.rs` reversed-range
  hard error.
- `renumber_rejects_truncated_relation_blob_payload` - chops one
  byte off the relation blob's PrimitiveBlock; pins
  `wire_rewrite.rs::count_varints_strict` defense.
- `cat_rejects_truncated_node_blob_payload` - chops one byte off
  the node blob's PrimitiveBlock; pins panic-freedom on adversarial
  node payloads (broader contract than just `scan_ids.rs` overflow,
  which would need a bespoke granularity-overflow fixture builder
  to target precisely).

**Remaining backlog** (still unlanded, no urgency): the additional
indexdata-trust sites listed in the original brief -
`renumber/pass1.rs:179`, `renumber/wire_rewrite.rs:272`,
`renumber/stage2.rs:226-231`, `altw/external/stage4.rs:438-478`,
`apply_changes/scanner.rs:162,188`, `apply_changes/streaming.rs:496`,
`commands/inspect/show_element.rs:53-57`. Each site can pick up a
regression test using the shipped primitives when a regression
appears or that area gets touched.

### T03 - Negative-ID / signed-arithmetic matrix [LANDED 2026-04-26]

`tests/common/mod.rs` ships three generators in canonical OSM order
(`-1, -2, ..., -n_neg, 1, 2, ..., n_pos` per kind):

- `generate_nodes_with_negatives(n_neg, n_pos)`
- `generate_ways_with_negatives(n_neg, n_pos, refs_per_way)`
- `generate_relations_with_negatives(n_neg, n_pos, members_per_rel)`

`tests/cli_negative_id_invariants.rs` runs the mixed-sign fixture
through six commands via `CliInvoker` and pins assertions matching
each command's documented contract in `DEVIATIONS.md`
("Negative input IDs rejected project-wide"):

- `renumber_rejects_mixed_sign_ids_with_named_id` - non-zero exit
  + stderr contains both `non-negative` (the error class) and `-1`
  (the specific offending id). Pins the documented contract: the
  three named entry points (`reframe_dense_with_new_ids`,
  `reframe_ways_with_new_ids`, `rewrite_relations_with_new_ids`)
  each return an error naming the offending id.
- `cat_preserves_mixed_sign_ids` - all 2N ids survive passthrough
  (cat is NOT named as an enforcement site; current behavior is
  passthrough, documented in TODO.md as a candidate for promotion
  to clean error).
- `inspect_handles_mixed_sign_ids` - no panic.
- `sort_preserves_mixed_sign_ids` - all 2N ids survive sort
  (same status as cat).
- `tags_filter_handles_mixed_sign_ids` - no panic. **Finding:**
  tags-filter silently drops negative-id ways through its
  parallel-classify path. Aligned with the project-wide
  "negatives shouldn't be in production PBFs" stance, but
  inconsistent with renumber's clean-error shape; tracked in
  TODO.md alongside cat/sort/inspect/getid.
- `getid_addresses_negative_ids` - no panic on `n-1,n-2,w-1`.

The diff/derive parallel-shard sites
(`diff/parallel.rs:138-142,354-357,384`,
`derive_parallel.rs:136-142`) and `geocode_index/builder/pass1_5.rs`
are guarded by `debug_assert!` only. `brokkr check` runs in
release mode (`cargo test --release`) so these planner asserts
are NOT exercisable from the CLI sweep; they fire under
`cargo test` (debug profile) when present in the upstream chain.
Per the user-decision constraint to skip lib plumbing in this
batch, that gap stays open.

### T04 - Adversarial / truncated-input tests [LANDED 2026-04-26]

`tests/cli_truncation_sweep.rs` ships `truncation_sweep_no_panic`,
which:

1. Builds a small ~6-blob synthetic PBF via
   `write_multi_block_test_pbf`.
2. Computes ~20-30 truncation offsets covering every blob's
   length-prefix midpoint, header midpoint, header end, payload
   midpoint, payload end, plus 12 uniform offsets across the file.
3. For each offset, drives `cat`, `inspect`, and `sort` through
   `CliInvoker` with a per-invocation 8 s timeout.
4. Asserts each invocation finishes without `panicked at` in stderr
   and bounded stderr size (catches multi-GB allocation explosions).

The proptest baseline (T07) covers the parse-never-panics half;
this sweep covers the command-level surface.

**Sites covered:** `read/header_walker.rs:149-164`,
`read/raw_frame.rs:65-67,124-127`, `scan/classify.rs:59-95,110-163`,
`renumber/wire_rewrite.rs:486-491`, and the two geocode bucket-file
truncation findings - all reachable through the cat/inspect/sort
fan-out.

### T05 - `-j N` vs `-j 1` parity matrix [PARTIAL - LANDED 2026-04-26]

Diff, derive-changes, and apply-changes already had parity tests
landed during the cluster-2 sweep (predates this batch):

- `diff_block_pair_parallel_matches_sequential_on_multi_blob`
  (`tests/cli_diff.rs:851`) - `-j 1` vs `-j 4`, asserts text + stats
  parity.
- `derive_changes_jobs_parity_roundtrips_to_same_output`
  (`tests/cli_derive_changes.rs:546`) - derive at `-j 1` and `-j 4`,
  apply both back, assert all four PBFs are element-equivalent.
- `merge_jobs_parity_on_multiblob_input`,
  `merge_jobs_parity_without_locations_on_ways`
  (`tests/cli_apply_changes_invariants.rs:246,273`) - `-j 2` vs
  `-j 4` (apply-changes rejects `-j 1` for deadlock reasons),
  asserts stats summary parity.

Combined with the existing `inspect --nodes`, `tags-filter` two-pass,
and `tags-count` parity tests, the three commands whose lib API
exposes `jobs` are fully covered.

**Deferred (need new lib API):**
- altw external stage 4 worker count (currently hard-coded)
- geocode Pass 1.5 / Pass 3 Stage A parallel degree
- `check --refs` (T09)

These three remain on the backlog. The 2026-04-26 batch deliberately
skipped lib-plumbing to keep the test-infrastructure sprint from
fanning out into pipeline rewrites; a future PR can plumb a `jobs`
arg through whichever pipeline becomes the focus and add the matching
parity test then.

### T06 - Scratch-dir / temp-file cleanup invariants [LANDED 2026-04-26]

`tests/common/mod.rs` ships `with_tracked_scratch_dir(scratch_root,
expected_new_paths, f)`. Internally:

1. Snapshot `scratch_root` before `f` runs.
2. Run `f`.
3. Snapshot `scratch_root` after `f` returns.
4. Remove every path in `expected_new_paths` from the post-snapshot.
5. Call the existing `assert_scratch_unchanged` to pin no-leak.
6. Return `f`'s result.

Helper-only landing - no caller migration in this batch. The
existing fault-injection tests still inline the `snapshot_dir +
assert_scratch_unchanged` pattern (correctly so; their pre/post
sequencing is hand-coded around panic boundaries). Future tests
that aren't fault-injection-shaped but still want scratch-dir
assertions can adopt the helper directly.

### T07 - Property-based testing via `proptest` [LANDED 2026-04-26]

`tests/proptests.rs` ships four properties at 64 cases each:

- `primitive_block_from_arbitrary_bytes_never_panics` -
  `PrimitiveBlock::new(Bytes::from(arbitrary))` returns Ok or Err,
  never panics.
- `blob_reader_arbitrary_bytes_never_panics` -
  `BlobReader::new(Cursor::new(arbitrary))` walks finite Ok/Err
  sequence (capped at 32 items), never panics.
- `blob_reader_truncated_fixture_never_panics` - truncating a
  known-good fixture at any byte offset never panics the reader.
- `node_fixture_roundtrips` - arbitrary count + start_id → write →
  read back → id sets match.

`proptest = "1"` is in `[dev-dependencies]`;
`proptest-regressions/` is gitignored to avoid committing
case-specific reproducers.

**Backlog deferred to a future PR:**

- `parse_osc_file` proptest - the symbol takes a `&Path`, not bytes;
  needs a small wrapper that writes bytes to tempdir and parses, or
  a different entry point.
- apply/derive inverse property - needs more scaffolding than fits
  this batch.
- Header flag combinations - small additional set, low priority.

### Follow-ups from 2026-04-26 batch review

After T02/T03/T04/T05/T06/T07 landed (commits `66f2fb7`, `daf5a5b`,
`9eb37a8`, `b245835`), four reviewers audited the result: two
internal Opus agents (brief-vs-delivered, contract-coverage) plus
two external reviews. Findings are tracked here, grouped by tier.
Provenance markers: `[A1]` / `[A2]` for the two internal Opus
agents, `[R1]` / `[R2]` for the two external reviews, `[all]` when
multiple reviewers cross-confirmed.

### Tier A: assertion strengthening in delivered tests

Each item is a regression that would slip past the current test.
Fixes are small (1-3 lines per item) and live in already-shipped
test files.

**T02 - cluster-2 regression assertions:**

- A1 - `altw_external_rejects_reversed_indexdata_range`
  (`tests/cli_defensive_input.rs:294-297`) uses
  `stderr.contains("reversed indexdata range") || stderr.contains("max_id")`.
  The OR defeats the contract: the actual error always contains
  both substrings, so the disjunction can't distinguish "fix in
  place" from any unrelated error mentioning max_id. **Drop the
  OR**, pin only `reversed indexdata range`. `[all]`
- A2 - `renumber_rejects_truncated_relation_blob_payload`
  (`tests/cli_defensive_input.rs:354-356`) asserts only
  "non-zero exit + no panic". **Outcome 2026-04-26**:
  strengthening to require the
  `"reframe_relations ... memids|types"` substring failed - the
  whole-block last-byte chop lands outside memids/types in the
  current fixture, so renumber rejects via an upstream protobuf
  walk before reaching `count_varints_strict`. Surgically
  pinning `count_varints_strict` requires a mutation primitive
  that finds and truncates the memids byte string specifically.
  **Deferred follow-up**: extend `tests/common/adversarial.rs`
  with a `truncate_relation_memids(pbf, blob_idx, relation_idx)`
  primitive (~30 lines walking PrimitiveBlock -> PrimitiveGroup
  -> Relation field 9). Then strengthen this test to assert the
  memids/types substring. `[all]`
- A3 - `cat_rejects_truncated_node_blob_payload`
  (`tests/cli_defensive_input.rs:395-403`) explicitly does NOT
  assert exit status. **Outcome 2026-04-26**: strengthening to
  `assert!(!out.status.success())` revealed cat exits 0 on this
  truncation - cat tolerates partially-readable blobs by design
  (the original test comment was correct). The broader
  "non-zero exit on truncation" contract lives in the truncation
  sweep where the cut lands at frame boundaries the reader
  detects. Pinning `!success` here would force a code change to
  cat's tolerance policy, deferred. Comment in the test now
  documents the finding. `[all]`

**T03 - negative-id sweep assertions:**

- A4 - `cat_preserves_mixed_sign_ids`
  (`tests/cli_negative_id_invariants.rs:56-89`) builds negative
  relations (via `generate_relations_with_negatives`) but
  validates only node and way ids. Relation-passthrough
  regression slips past. **Add the relation id parity check**
  matching the node/way pattern. `[R1]`
- A5 - `sort_preserves_mixed_sign_ids`
  (`tests/cli_negative_id_invariants.rs:138-146`) checks
  neg/pos preservation only for nodes; ways and relations get
  count-only assertions. Sign loss with unchanged counts passes.
  **Add neg/pos filter checks for ways and relations** matching
  the node block. `[R1]`
- A6 - `getid_addresses_negative_ids`
  (`tests/cli_negative_id_invariants.rs:236-258`) gets a `-o`
  output file but never reads it back. A regression that drops
  every queried negative id silently passes. **Add output read +
  assert the queried ids appear**. `[A1, R1]`
- A7 - `tags_filter_handles_mixed_sign_ids`
  (`tests/cli_negative_id_invariants.rs:184-189`) surfaced the
  silent-drop finding (committed in `daf5a5b`'s message,
  documented in `TODO.md`) but pins NOTHING about it. A future
  regression that flips the silent-drop to silent-pass-through
  passes silently. **Lock the current behavior**: read output,
  assert `n.ways.iter().filter(|w| w.id < 0).count() == 0` with
  a comment pointing at the TODO entry. When TODO is acted on,
  the test fails and forces a deliberate update. `[A1, A2, R2]`

**T04 - truncation sweep assertions:**

- A8 - `run_and_assert_no_panic`
  (`tests/cli_truncation_sweep.rs:93-112`) and
  `run_sort_and_assert_no_panic` (`:114-127`) never assert
  exit status. **Outcome 2026-04-26**: strengthening to require
  `!success` revealed all three commands (cat, inspect, sort)
  are tolerant by design - they decode what they can and exit
  0 even on truncated input. The reviewer brief over-stated
  the contract. Helper now pins "no panic + bounded stderr" for
  every command; the `!success` half is deferred until a
  decision is made about whether to promote any of these to
  strict-error-on-truncation. `[all]`
- A9 - `run_sort_and_assert_no_panic`
  (`tests/cli_truncation_sweep.rs:114-127`) omits the
  `stderr.len() < 100_000` bounded-stderr check that the
  cat/inspect helper enforces. Sort gets weaker coverage than
  the brief's "no multi-GB allocation" applies uniformly.
  **Add the bounded-stderr assertion**. `[all]`
- A10 - `step_by(...).take(30)` at
  `tests/cli_truncation_sweep.rs:73-79` drops boundaries on the
  ~6-blob fixture - a boundary-specific regression at, say,
  `b.header_end - 1` of blob 4 may be sampled out. **Either
  remove the sampling cap** (the structural offset list is ~50
  for a 6-blob fixture, all invocations finish in < 1 s; the
  cap is over-conservative), **or shrink the fixture** so all
  offsets fit under the cap budget. `[R1]`

**T07 - proptest assertions:**

- A11 - `node_fixture_roundtrips`
  (`tests/proptests.rs:99-129`) asserts ID-set equivalence
  only - not coordinates, tags, metadata. Coordinate-corruption
  regressions silently pass. **Replace the id-set assertion
  with `assert_elements_equivalent`** (the helper already
  exists at `tests/common/mod.rs:975`). Same fix applies to
  `negative_id_node_fixture_roundtrips`. `[all]`
- A12 - No way / relation roundtrip property exists, and the
  T07 deferred backlog (testing.md "Skipped from T07's brief")
  does not list them. **Either add the properties** (~20 lines,
  same shape as `node_fixture_roundtrips`) **or list them in the
  T07 deferred section** with a one-line justification. `[R1]`

### Tier B: documented contract gaps in pre-batch coverage

Pre-batch tests already cover most of `DEVIATIONS.md` and
`CORRECTNESS.md`, but the review surfaced these gaps. Each is a
small new test in an existing file.

- B1 - **Null Island real `(0, 0)` node** [LANDED 2026-04-26].
  `null_island_real_node_treated_as_missing` in
  `tests/cli_add_locations_to_ways.rs`. Pins the documented
  CORRECTNESS limitation: a real node at `(0, 0)` is reported
  as missing because every coordinate index uses `(0, 0)` as
  the absent sentinel. If a future fix adds an occupancy
  bitmap, this test fails and forces a deliberate update to
  CORRECTNESS.md.
- B2 - **altw `--index-type external` missing-nodes**
  [LANDED 2026-04-26].
  `missing_node_refs_get_zero_coordinates_external` in
  `tests/cli_add_locations_to_ways.rs`. Mirrors the dense and
  sparse twins. External requires indexed input
  (`write_indexed_pbf` rather than `write_test_pbf`).
- B3 - **renumber relation-member orphan preservation**
  [LANDED 2026-04-26].
  `renumber_orphan_relation_member_preserves_old_id` in
  `tests/renumber_external.rs`. Asserts orphan member ids
  (way 99999 + node 99999) survive with their old ids while
  in-input refs are remapped.
- B4 - **renumber negative relation-member ref rejection**
  [LANDED 2026-04-26].
  `renumber_rejects_negative_relation_member_ref` in
  `tests/renumber_external.rs`. Asserts the error message
  flags the negative requirement and names the offending id
  (`-7`).
- B5 - **Geocode admin u16 entry cap** [LANDED 2026-04-26].
  `build_rejects_admin_entry_count_over_u16_max_for_one_cell`
  in `tests/geocode_index.rs`. Fixture: 65 536 admin
  relations all sharing one outer ring → all polygons land in
  the same S2 cell → triggers the per-cell admin cap at
  `src/geocode_index/builder/admin.rs:227`.

### Tier C: reviewer disagreements (verified 2026-04-26)

- C1 - **Diff content-equality "metadata-ignored" half**
  (`DEVIATIONS.md:50-55`). **Verified**: pinned by
  `cli_diff.rs:731` (`diff_pure_metadata_bump_is_common_not_modified`).
  R2 was correct; A2's proposed minimal-close test would have
  duplicated existing coverage.
- C2 - **Osmosis -1 sentinel** (`CORRECTNESS.md:53-87`).
  **Verified**: pinned by four tests in `tests/getid.rs`:
  `getid_normalizes_dense_node_version_minus_one_to_absent_metadata`
  (`:338`),
  `getid_normalizes_dense_node_changeset_minus_one_to_zero`
  (`:392`),
  `getid_normalizes_way_version_minus_one_to_absent_metadata`
  (`:446`),
  `getid_normalizes_way_changeset_minus_one_to_zero` (`:502`).
  Both tiers (dense + non-dense, both fields) covered.
  R2 was correct.

### Provenance and review prompts

The full prompts and reports from all four reviewers are not
checked into the repo (one-shot artifacts). The above is the
synthesis. Re-running similar reviews should target the same four
docs (`DEVIATIONS.md`, `CORRECTNESS.md`,
`decisions/0002-negative-ids-rejected-project-wide.md`, this
file) plus the test files.

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

### T11 - Brokkr validation profiles [LANDED 2026-04-25]

Landed via brokkr commits `f7a96b7` (initial `[test.profiles.*]` /
`[test.sweeps.*]`) and `2235792` (config redesign that consolidated
sweeps under the `[[check]]` array). Pbfhogg's `brokkr.toml` migrated
to the landed shape the same day.

The annotation surface is plain Rust module paths (`mod tier2`,
`mod platform`, `mod serial`) translated into cargo's substring
filter and `--skip` flag. `#[ignore]` stays a libtest mechanic for
tests that must never run accidentally - it is no longer the tier
label.

For command-family CLI tests, the current `tests/cli_<command>.rs`
shape splits intent inside the file:

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

Today only `cli_sort.rs`, `cli_apply_changes.rs`,
`cli_add_locations_to_ways.rs`, and `cli_cat.rs` carry `mod platform`
(for the `--direct-io` / `--io-uring` positive tests). No `mod tier2`
or `mod serial` populations yet - the fault-injection split made
serial tests per-binary and race-free, and tier-2 matrices grow when
commands actually need them rather than preemptively.

The landed `brokkr.toml` lives at the project root.

Command surface:

```text
brokkr check                  # tier 1 (default profile)
brokkr check --profile sort   # tier 2 command slice
brokkr check --profile full   # tier 3 in-project gate
brokkr check --profile platform
brokkr check --profile serial
```

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

Feature-surface assertions in CLI tests are now possible:
brokkr's `build_packages` (commit `f7a96b7`) plus
`BROKKR_TEST_BIN_DIR` (commit `b3aa444`) close the binary/library
feature-parity gap. The two `feature_missing_error` tests in
`tests/cli_sort.rs` are the canonical precedent.
