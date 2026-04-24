# Testing

Live tracker for pbfhogg's test infrastructure and coverage. Cross-ref
`reference/performance.md` for perf baselines and TODO.md's "Important:
ignored tests" section for the runbook on tests that don't run by default.

## Status summary

- **Test fixture infrastructure:** landed (2026-04-22). See [Landed](#landed) below.
- **Fault-injection harness:** **complete across all 8 parallel pipelines**
  (apply-changes streaming, parallel_writer, parallel_gzip, uring_writer,
  diff/parallel, derive_parallel, altw external stage 3, geocode Pass 3
  Stage A). Caught one real deadlock (apply-changes drain) and one real
  scratch leak (derive_parallel outer temp files); both fixed along the way.
- **Open work:** 10 items (T01–T10), loosely prioritized. Real-bug items
  first, then high-leverage test-shape infrastructure, then opt-in.

## Conventions

- **`test-hooks` Cargo feature.** Gates fault-injection hooks across every
  parallel pipeline. Off by default; enabled under `--all-features`
  (which `brokkr check` uses). Release builds never see the hook code.
- **Two hook shapes.** Per-instance field on a public config struct (used
  by apply-changes via `MergeOptions::panic_at_blob_seq`; race-free with
  sibling tests) vs. process-global static atomics (used by writer-pool
  and shard-parallel pipelines whose workers are spawned deep inside
  constructors; tests `#[ignore]`d to force single-threaded execution).
  Picker: per-instance when the pipeline has a public config struct on
  its entry path, static atomics otherwise.
- **Scratch tracking.** `tests/common/mod.rs` exports `snapshot_dir` and
  `assert_scratch_unchanged` for before/after comparisons around error
  paths.
- **Hook consolidation (deferred).** The static-atomic submodules across
  parallel_writer / parallel_gzip / uring_writer / diff-parallel /
  derive-parallel / altw-stage3 / geocode-pass3 are structurally
  identical (`PANIC_AT_*` + `*_COUNT` + `reset()`). If the underlying
  pipelines converge in a later refactor, fold these into a shared
  module. Not worth it yet - keep each per-module so tests are explicit
  about which pipeline they're injecting into.
- **Policy proposal (not-yet-adopted).** Every new parallel pipeline
  should ship with three tests: a worker-panic test, a `-j N` vs `-j 1`
  parity test, and a scratch-leak test. Bug density in the sweep skewed
  hard toward the three newest / biggest parallel subsystems, and T05 +
  T06 + T09 exist precisely because earlier pipelines didn't have this
  discipline from the start. Worth considering as a CI gate once the
  harness matures.

## Open work

Work item IDs are fixed and stable. Cite by ID in commits / ADRs /
other notes.

### T01 — `jobs == 1` apply-changes worker-panic deadlock

Real deadlock, not just a test gap. With a single worker and a worker
panic, the scanner blocks on a full `candidate_rx` (no one consuming),
the drain blocks waiting on senders, and the command hangs forever.
The fault-injection harness currently sidesteps this by running with
`jobs: Some(2)`.

**Fix shape:** plumb an `Arc<AtomicBool>` shutdown signal that the
scanner polls between sends, set from the worker scope's Drop path on
unwind. Requires threading through scope boundaries. Not blocking
0.3.0; re-arm the harness with `jobs == 1` once this lands to lock
the invariant.

### T02 — Lying-indexdata fixture primitives (extended coverage)

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

### T03 — Negative-ID / signed-arithmetic matrix

~8 findings mishandle negative element IDs because guards are gated on
indexdata or shard planners use raw numeric compare instead of
`osm_id_cmp`. Every current fixture uses non-negative IDs.

**Shape:** add `generate_nodes_with_negatives(start_neg, start_pos, n)`
plus way/relation equivalents to `tests/common/mod.rs`. Canonical OSM
order: `..., -3, -2, -1, 0, 1, 2, ...`. Run every command through the
mixed-sign fixture, including `-j N` variants.

The `renumber` deviation in DEVIATIONS.md says "negative inputs
rejected" — we currently only test the happy path with indexdata
present.

**Sites covered:**
- `renumber/pass1.rs:179`, `renumber/wire_rewrite.rs:272,519-524`
- `diff/parallel.rs:138-142,354-357,384`
- `derive_parallel.rs:136-142` + sibling emit/merge sites
- `geocode_index/builder/pass1_5.rs:102`

Pairs with T05 (`-j N` parity) for maximum coverage — the
shard-parallel bugs only surface on mixed-sign inputs.

### T04 — Adversarial / truncated-input tests

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

### T05 — `-j N` vs `-j 1` parity matrix

Existing parity coverage: `inspect --nodes`, `tags-filter` two-pass,
`tags-count`, `merge_jobs_parity_on_multiblob_input`.

**Missing:**
- `diff -j N`
- `derive-changes -j N`
- `apply-changes -j N` (beyond the single merge fixture)
- altw external stage 4 worker count (currently hard-coded; would
  need a library arg)
- geocode Pass 1.5 / Pass 3 Stage A parallel degree
- `check --refs` — blocked on T09

Same shape as the existing tests: element-equivalent output + matching
summary counters across worker counts. Pins regression of the
diff/derive shard numeric-compare family, the `OwnedBytes` counter
bug, and any future worker-count-dependent drift. Pair with T03 for
maximum coverage.

### T06 — Scratch-dir / temp-file cleanup invariants

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

### T07 — Property-based testing via `proptest`

Recommended first pass before any `cargo-fuzz` investment (T10). Same
class of bugs — parse crashes, boundary violations, roundtrip
asymmetries — but runs inside `cargo test` in seconds, no corpus
directory to gitignore, no long-running campaigns. Shrinks failing
inputs to minimal reproducers.

**Rough targets** (one `#[proptest]` fn each):

- `PrimitiveBlock::from_vec(bytes)` over arbitrary `Vec<u8>` — must
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

### T08 — Boundary-twin scan across modules

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

### T09 — Parallel-classify parity test for `check --refs`

Other three parallel-classify commands (`inspect --nodes`,
`tags-filter` two-pass, `tags-count`) got `jobs=1` vs `jobs=4` parity
tests via their `jobs: Option<usize>` / `jobs: usize` library APIs.
`check_refs` has no equivalent override in its public signature
(`src/commands/check/refs.rs:141`), so a parity test has to either
exercise the CLI via `cli/tests/cli.rs` (hard to observe worker count
from outside) or wait for a plumbed `jobs` argument.

Not urgent — worker-count-independent correctness is implicitly
covered by existing single-blob tests. Revisit if `check --refs` ever
grows a `jobs` flag. Unblocks the final entry in T05.

### T10 — Fuzz testing via `cargo-fuzz`

Optional follow-up to T07; only worth the setup if someone wants to
run weekend campaigns. PBF parsing (`PrimitiveBlock::from_vec`), OSC
parsing (`parse_osc_file`), and wire-format decoders (`Cursor`,
`WireBlock`, `WireInfo`) all accept untrusted input. Targets for these
entry points would catch panics, OOM, and logic errors on malformed
data. Also fuzz the roundtrip path (write → read → compare).

**Cost:** `fuzz/corpus/` grows to hundreds of MB — low GB per target
over long campaigns, and `fuzz/target/` is ~500 MB – 1 GB of build
artifacts. Both must be gitignored; a developer running the fuzzer
locally needs that space.

**Schedule:** smoke runs (60 s) only verify the harness; real
bug-hunting needs hours to days per target ("weekend campaign"
cadence). Skip until T07 exposes a gap that only coverage-guided
fuzzing can fill.

## Landed

Chronological, for context on what's already solved. Not a work
backlog.

- **Test fixture infrastructure** (2026-04-22). `TestNode` / `TestWay` /
  `TestRelation` extended with `meta: Option<TestMeta>` (default
  `None`); ~428 struct literals across 14 test files migrated via a
  one-shot script (script deleted post-migration — migration is
  idempotent; literals are the source of truth). New helpers:
  `write_multi_block_test_pbf(path, nodes, ways, rels, block_size)`
  for multi-blob fixtures without needing 8000+ elements per type,
  `generate_nodes` / `generate_ways` / `generate_relations` for
  sequential id-sorted vectors, `assert_indexed` /
  `assert_non_indexed` for blob-header assertions. Smoke-tested in
  [`tests/fixture_helpers.rs`](../tests/fixture_helpers.rs).

- **altw external fd footprint** (2026-04-23). Stage 1 pass B held
  `num_workers * NUM_BUCKETS` rank-shard files open concurrently
  (~4352 fds at 17 workers), tripping Linux default soft ulimit
  (1024; some distros cap hard at 4096) with `EMFILE`. Fix:
  self-raise `RLIMIT_NOFILE` soft to hard cap at the top of
  `stage1_way_pass` (unprivileged, free), then cap `num_workers` at
  `(fd_budget - 64_headroom) / NUM_BUCKETS`. If even one worker's
  256-shard budget can't fit, fails clean with a `ulimit -n N` hint.
  New counters: `extjoin_nofile_soft_cap`, `extjoin_cpu_cap_workers`,
  `extjoin_fd_cap_workers`. `backend_parity_dense_sparse_external_auto`
  un-ignored and passes under default ulimit in both feature sweeps.

- **apply-changes `-j N --locations-on-ways` consumer-build drain
  invariant** (2026-04-23). The false-positive path unconditionally
  emits `DrainItem::CopyRange`, which the consumer build (no
  `linux-direct-io`, `use_copy_range=false`) rejects. Fix: thread
  `use_copy_range` through `StreamingConfig` → worker; when false,
  route false-positives through the owned-passthrough path
  (`handle_owned_passthrough`, pread the full frame, emit
  `DrainItem::OwnedBytes`). `merge_jobs_parity_on_multiblob_input`
  now passes in both feature sets. Three merge.rs stats tests
  (`merge_gap_creates_between_blobs`, `merge_stats_accuracy`,
  `merge_type_transition_node_to_relation_skipping_ways`) now run to
  completion in consumer; they still fail stats assertions because
  `DrainItem::OwnedBytes` does not credit per-kind `base_*` counts
  (only `CopyRange` does) — tracked as a separate stats-drift gap.

- **Fault-injection harness** (2026-04-24, eight commits). Hook
  infrastructure + one canonical test per parallel pipeline.

  | Pipeline | Hook shape | Canonical test | Bug surfaced? |
  |---|---|---|---|
  | apply-changes streaming | Per-instance (`MergeOptions::panic_at_blob_seq`) | `fault_injection_worker_panic_surfaces_error_and_leaves_scratch_clean` | **Yes** — drain deadlock (loop only exited when reorder buffer empty; panic left stuck seqs). Fixed by breaking on channel-disconnect unconditionally. |
  | parallel_writer | Static atomic (`PANIC_AT_POOL_OP_COUNT`) | `fault_injection_parallel_writer_pool_panic_surfaces_error` | No |
  | parallel_gzip | Static atomic (`PANIC_AT_POOL_OP_COUNT`) | `fault_injection_parallel_gzip_worker_panic_surfaces_via_finish` | No |
  | uring_writer | Static atomic (`PANIC_AT_DISPATCH_COUNT`) | `fault_injection_uring_writer_dispatch_panic_surfaces_via_flush` | No; test gracefully skips on hosts with `RLIMIT_MEMLOCK < 16 MB` |
  | diff/parallel | Static atomic (`PANIC_AT_SHARD_IDX`) | `fault_injection_diff_parallel_shard_panic_surfaces_and_sweeps_scratch` | No |
  | derive_parallel | Static atomic (`PANIC_AT_SHARD_IDX`) | `fault_injection_derive_parallel_shard_panic_surfaces_and_sweeps_scratch` | **Yes** — outer aggregate temp files (`derive-par-{creates,modifies,deletes}-{pid}.xml.tmp`) not cleaned up on error-path early-returns. Fixed via `PathGuard::file()` wrappers per ADR-0003. |
  | altw external stage 3 | Static atomic (`PANIC_AT_BUCKET_IDX`) | `fault_injection_altw_stage3_bucket_panic_surfaces_and_cleans_scratch` | No; stage 3 panic also exercises stage-4 recovery via router abort |
  | geocode Pass 3 Stage A | Static atomic (`PANIC_AT_STREETS_WAY_IDX`) | `fault_injection_geocode_pass3_streets_panic_sweeps_bucket_dirs` | No |
