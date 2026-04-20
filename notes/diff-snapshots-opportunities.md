# `diff-snapshots` optimization plan

## Scope

`brokkr diff-snapshots` compares two independent PBF snapshots (no
byte-level overlap, full decode both sides). Drives both `pbfhogg diff`
(human-readable output) and `pbfhogg diff --format osc`
(OSC XML via `derive_changes`). The code path shared by both is
`block_pair_merge_phase` in `src/osc/merge_join.rs`.

This document supersedes the "diff v3: non-overlapping block skip" TODO
entry - v3 is one of several items here, not the whole plan.

## Baselines

### Planet (ratification dataset, 35-min turnaround)

Planet 47-day snapshot pair (`base` vs `snapshot.20260411`), commit
`7e9c2e9`, `plantasjen`:

| Command                               | UUID       | Wall      |
|---------------------------------------|------------|-----------|
| `diff-snapshots` (human-readable)     | `42aedca1` | 2150.9 s (35m50s) |
| `diff-snapshots --format osc`         | `53900d5f` | 2225.6 s (37m06s) |

Not used for iteration - reserved for final ratification.

### Germany (iteration dataset, ~100-s turnaround)

Germany 55-day snapshot pair (`base` = 2026-02-24 seq 4704 vs
`snapshot.20260420` seq 4957, 4.5 GB raw / 4.7 GB indexed), commit
`e27c89e`, `plantasjen`:

| Command                           | UUID       | Wall    |
|-----------------------------------|------------|---------|
| `diff-snapshots` (`--bench 1`)    | `22e4f65f` | 103.3 s |

Phase split (NODE 65 %, WAY 33 %, REL 1 %) is proportionally similar
to planet (74 / 26 / 0.7 %); single-threaded signature
(`avg_cores=1.0`, `user=1.0 kern=0.0`, `peak_threads=1`) is identical.
Germany is used for the optimization loop; re-measure on planet only
at milestones.

## Measured phase breakdown (2026-04-19, `a9d430f2`)

First run with the shadow counters + phase markers + hotpath
annotations landed. Commit `052da8b`, `plantasjen`, `--bench 3`:

**Wall: 2134.3 s (35m34s).** Within noise of the `42aedca1` baseline.

| Phase                        | Duration  | % of wall | Avg Cores | Peak Anon | Disk Read |
|------------------------------|-----------|-----------|-----------|-----------|-----------|
| `DIFF_PHASE_NODE_START/END`  | 1572.6 s  | 73.7 %    | 1.0       | 41.4 MB   | 121.0 GB  |
| `DIFF_PHASE_WAY_START/END`   |  547.5 s  | 25.7 %    | 1.0       | 37.8 MB   |  57.8 GB  |
| `DIFF_PHASE_REL_START/END`   |   14.1 s  |  0.7 %    | 1.0       | 28.2 MB   |   1.7 GB  |

All three phases: `user=0.9 kern=0.0`, `peak_threads=1`,
`majflt=0`. Single core, userspace, nothing waiting on disk or
page-faulting. Aggregate disk read 180.5 GB over 2134 s = ~85 MB/s
effective, an order of magnitude below the NVMe ceiling.

### Shadow counters

```
pairs_byte_equal               = 0
elements_byte_equal            = 0
pairs_overlapping_decoded      = 3
elements_overlapping_decoded   = 546 473
blobs_old_only                 = 0
elements_old_only              = 0
blobs_new_only                 = 440
elements_new_only              = 109 463 424

diff_common   = 11 587 352 039  (98.8 %)
diff_created  =    109 478 210  (0.9 %)
diff_deleted  =      9 139 976  (<0.1 %)
diff_modified =     30 961 245  (0.3 %)
```

### What the measurement resolves

- **The entire wall is single-core user CPU across two reader streams.**
  Disk is not the ceiling, memory is not the ceiling, context switches
  are only vol_cs from channel waits. Decompress + protobuf decode +
  the sequential merge loop saturate one core.
- **v1 byte-equal fast path never fires.** `pairs_byte_equal = 0` across
  the full run. Two independent PBFs with different compression metadata
  mean no blob is literally byte-identical, even when its elements are.
  The optimization does not apply to the `diff-snapshots` workload - it
  was designed for the `diff` workload (apply-changes internally first,
  then diff against the same input).
- **v3 non-overlapping block skip is not a planet-relevant item.**
  `blobs_old_only = 0` across all three phases. `blobs_new_only = 440`
  (all concentrated in the trailing end of new-side node and way files).
  440 out of ~300 K blob pairs ≈ 0.15 % of the population - skipping
  these saves at most 0.15 % of the wall. Close the item.
- **Type-phase split matches priority:** the node phase is 74 % of the
  wall on its own. Any parallel-decode work should land there first;
  way phase second; rel phase is a rounding error.

## Hotpath breakdown (2026-04-20, germany `e45f81c4`)

First `--hotpath` run on germany after fixing the CLI destructor-skip
bug (see "Infrastructure fixes" below). Commit `a3795c2`:

| Function                                      | Calls    | Total    | % of wall |
|-----------------------------------------------|----------|----------|-----------|
| `pbfhogg::main`                               | 1        | 105.24 s | 100.0 %   |
| `diff::diff_block_pair`                       | 1        | 105.23 s |  99.99 %  |
| `merge_join::block_pair_merge_phase`          | 3        | 105.23 s |  99.99 %  |
| **`merge_join::merge_decoded_pair`**          | 124 842  | **71.30 s** | **67.75 %** |
| `merge_join::element_merge_pair`              | 124 842  |  71.28 s |  67.73 %  |
| **`merge_join::decode_pending`**              | 125 341  | **31.02 s** | **29.48 %** |
| `read::blob::decompress_into`                 | 125 341  |  28.98 s |  27.54 %  |
| `read::block::from_vec_with_scratch`          | 125 341  |   2.03 s |   1.93 %  |
| `merge_join::drain_remaining`                 | 3        |   1.69 s |   1.61 %  |

**The decode path is not the dominant cost.** Zlib decompress plus
protobuf parse is only 29.5 % of the wall; the per-element merge loop
inside `element_merge_pair` is 67.7 %. Any parallel-decode-only
arrangement caps at roughly 1.15 × wall reduction on this workload.

**Call-count ratio is informative.** `decode_pending` fires 125,341
times across 124,842 merge iterations - almost exactly one decode per
merge, not two. That means residual churn dominates the pair pattern:
the typical iteration keeps one side's decoded block as a residual and
decodes a fresh block only on the opposite side. Fast-path "both
sides freshly decoded" rarely fires after the first pair per phase.
Implication: a decode-parallelism plan that prefetches both sides
independently (treating them as independent streams) over-decodes
roughly 2 ×, since the consumer only needs one fresh block per
iteration.

**Per-element cost within `element_merge_pair`.** 71.3 s / ~2 × 10⁹
element operations ≈ 35 ns per element-pair step. That includes:
- Two `DenseNodeIter::next()` advances (3-4 varint decodes each for
  id/lat/lon deltas + 1-N varint tag scan)
- `element_id()` enum match × 2 for ID comparison
- For the 98.8 % `Equal` case: `borrowed_elements_equal` which
  re-walks the tag iterator for equality
- `on_action(BlockMergeAction::...)` via `&mut dyn FnMut`, dynamic
  dispatch on every element
- `Result<(), Box<dyn Error>>` propagation through `?` on every call

## Infrastructure fixes (landed en route)

**CLI destructor skip breaking `--hotpath`.** `cli/src/main.rs::main`
used `process::exit(1)` inside six helpers (`run_diff`, `run_check`,
`run_show_element`, `run_inspect` × 2, top-level error handler).
`process::exit` terminates the process without running destructors,
so the `HotpathGuardBuilder` guard at the top of `main` never got a
chance to flush its JSON report. Every `diff-snapshots --hotpath` run
before commit `a3795c2` recorded mode=hotpath with zero function rows
and brokkr logged `failed to read hotpath report`. Fixed by returning
`process::ExitCode` from `main` and carrying non-zero exit status via
a small `ExitWithCode(u8)` sentinel on the error channel; main's
error handler downcasts and maps to `ExitCode::from(u8)`. The guard
drops as main returns. Diagnosis from reviewer round, ratified by
measurement: the next `--hotpath` run produced the breakdown table
above.

## Target: ~8 min (aspirational, not a costed plan)

8 min is extrapolated from the `renumber` 58 min → 3m14s result (18×),
not derived from a known pipeline. The measured hotpath numbers below
partition the actual ceiling.

### Parallelism ceiling (revised after hotpath)

The wall splits roughly:

| Component                       | Wall %   | Parallelisable? |
|---------------------------------|----------|-----------------|
| `element_merge_pair` (merge)    | 67.7 %   | Per pair - blocked by residual churn |
| `decode_pending` (decompress)   | 29.5 %   | Per decode task - trivially concurrent |
| Other (classify, emit, drain)   |  2.8 %   | Serial consumer, but small |

Three Amdahl regimes, from least to most parallelism (planet wall):

| Scenario                                | Node  | Way   | Rel | Total  | Speedup |
|-----------------------------------------|-------|-------|-----|--------|---------|
| Today (baseline)                        | 1572s |  547s | 14s | 2134s  | 1.0×    |
| **Decode only** parallelised (6 cores)  | 1252s |  436s | 14s | 1702s  | 1.25×   |
| **Merge only** parallelised (6 cores)   |  639s |  222s | 14s |  875s  | 2.44×   |
| **Both** parallelised (6 cores)         |  319s |  111s | 14s |  444s  | 4.81×   |
| **Both** parallelised (8 cores, ideal)  |  239s |   83s | 14s |  336s  | 6.35×   |

"Decode only" ≈ the original plan before hotpath data. Hits a hard
ceiling at ~1.25× because the 68 % merge cost stays serial.

Reaching the 8-min aspirational target requires parallelising BOTH
decode and merge. The 6-core "both parallel" case (444 s ≈ 7m24s)
is the first one that qualifies.

**Germany iteration projection at the same speedups:**

| Scenario                          | Germany total | Speedup |
|-----------------------------------|---------------|---------|
| Today                             |  103 s        | 1.0×    |
| Decode only (6 cores)             |   82 s        | 1.26×   |
| Merge only (6 cores)              |   42 s        | 2.45×   |
| Both (6 cores)                    |   21 s        | 4.90×   |
| Both (8 cores, ideal)             |   16 s        | 6.44×   |

## Measurement prerequisites

**Shadow counters (shipped 2026-04-19)** in
`src/osc/merge_join.rs::BlockPairMergeStats`, emitted from `diff` and
`derive_changes` after all three kind-phases complete. Counter names
all prefixed `mergejoin_shadow_`:

| Counter                          | Meaning |
|----------------------------------|---------|
| `pairs_byte_equal`               | v1 fast-path hits (no decode). |
| `elements_byte_equal`            | Sum of element counts for v1 hits. |
| `pairs_overlapping_decoded`      | Overlapping pairs that decoded both sides. |
| `elements_overlapping_decoded`   | Both-sides element count sum for overlapping decode. |
| `blobs_old_only` / `elements_old_only`   | BlobOldOnly emits (entirely single-sided) - v3 skip candidates. |
| `blobs_new_only` / `elements_new_only`   | BlobNewOnly emits - v3 skip candidates. |

**Phase markers:** `DIFF_PHASE_<NODE|WAY|REL>_START/END` and
`DERIVECHANGES_PHASE_<NODE|WAY|REL>_START/END`. Plus the existing
`DIFF_SCAN_START/END` and `DERIVECHANGES_SCAN_START/END` wrapping
the whole run. Lets `brokkr sidecar --durations` attribute the 2107 s
wall across the three type phases.

**Hotpath annotations:** `#[cfg_attr(feature = "hotpath",
hotpath::measure)]` on `diff_block_pair`, `diff_element_stream`,
`derive_changes_block_pair`, `derive_changes_element_stream`,
`collect_phase_block_pair`, `block_pair_merge_phase`. `brokkr <cmd>
--hotpath` attributes CPU time to these functions.

First measured run: `a9d430f2` (commit `052da8b`, 2026-04-19, --bench 3).
Rerun after any landed item:

```
brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 3
brokkr sidecar <UUID> --human
brokkr sidecar <UUID> --durations
brokkr sidecar <UUID> --counters --human
```

## Ranked opportunities

### 1. Parallel blob-pair merge (headline item, the only path to the target)

**Problem.** `element_merge_pair` is 67.7 % of the wall and runs on
the main thread inside `block_pair_merge_phase`. Each call processes
one blob pair's overlapping ID range, peeks/advances two iterators,
and calls `borrowed_elements_equal` on 98.8 % of elements. ~2 × 10⁹
element operations across 124,842 pair merges.

**Why it's hard: residual churn serialises pairs today.** After each
`merge_decoded_pair`, one side (the one with larger max_id) carries
forward as a residual; the next iteration decodes only the other side.
Pair N+1's shape (which side is fresh, which side is residual, and
where the skip cursors are) depends on pair N's outcome. You cannot
naively fan out 125K pairs to workers because the pair boundaries
are a function of the sequential walk.

**Chosen shape: pre-aligned pair plan, then parallel dispatch.**

Run a cheap pre-pass that reads both sides' indexdata only (no
decode) and produces a list of independent merge tasks. Each task
carries: the two `BlobIndex`es, a shared `merge_up_to` cutoff, and
pre-computed skip offsets. Tasks are independent because the walk is
deterministic from the indexdata alone.

```
Pre-pass (on consumer thread, cheap - reads only indexdata):
┌──────────────────────────────────────────────────────┐
│  Walk old and new indexdata streams in parallel      │
│  - Classify each step: disjoint / overlapping        │
│  - For overlapping: compute merge_up_to and decide   │
│    which side accumulates a residual                 │
│  - Emit a MergeTask { old_blob_idx, new_blob_idx,    │
│    old_skip, new_skip, merge_up_to } per step        │
│  - For single-sided: emit SinglesidedTask            │
└────────────────┬─────────────────────────────────────┘
                 │
                 ▼
         ┌──────────────────────────────────────────┐
         │  Rayon pool: N workers                   │
         │  Each task:                              │
         │    1. Read the two blobs via pread       │
         │    2. Decode both (~3.5 ms each)         │
         │    3. Run element_merge_pair             │
         │    4. Buffer emitted actions in a        │
         │       per-task Vec<OwnedActionLine>      │
         └────────────────┬─────────────────────────┘
                          │
                          ▼
         ┌──────────────────────────────────────────┐
         │  Main thread: collect results in task    │
         │  order, emit to stdout + update stats    │
         └──────────────────────────────────────────┘
```

Why this works:
- **Indexdata alone is enough to classify.** `BlobIndex` carries
  `min_id`, `max_id`, `count`, and `kind`. That is everything the
  classify branch in `block_pair_merge_phase` needs to decide
  disjoint vs overlapping, which side carries residual, and the
  merge_up_to cutoff.
- **pread-from-workers breaks the decode serialisation.** Each worker
  opens its own FD to each file (or uses pread on a shared FD) and
  reads the two blobs at known offsets. No shared `BlobReader`
  cursor.
- **Tasks are independent.** The merge_up_to cutoff partitions each
  blob into a range that belongs to this task's merge; skip offsets
  tell the element iterator where to start. A residual becomes
  "task K+1 starts old at skip=S in blob X" rather than "keep the
  decoded block around".
- **Output order preserved via task index.** Tasks dispatched in
  index order, results collected in index order via a reorder buffer
  on the main thread.

What's expensive in the pre-pass: reading both indexdata streams
sequentially, ~125 K iterations, each ~O(1) work. Estimated <1 s on
germany, <20 s on planet. Cost is one-shot per run, not per-pair.

What's cheap in workers: pread of two ~64 KB blobs (~0.1 ms each on
NVMe) + decode (~3.5 ms each) + merge (~0.5 ms). Total ~7 ms per
task. At 125 K tasks × 7 ms ÷ 8 workers = ~110 s of CPU work spread
across workers. Germany 103 s → ~15-20 s on an 8-core host.

**What changes in code:**
1. New `src/osc/merge_join/plan.rs` (or inline in merge_join):
   walks both sides' `BlobReader` with `set_parse_indexdata(true)`,
   driven purely by blob index metadata. Emits `Vec<MergeTask>`.
2. Parallel driver in `block_pair_merge_phase` replacement: spawn
   rayon workers that pread + decode + merge, buffer per-task
   actions in an owned `Vec<OwnedAction>` enum.
3. Per-task action buffers replace the `&mut dyn FnMut(&BlockMergeAction)`
   callback - the callback can't cross threads because actions borrow
   from the decoded block. Workers convert to owned equivalents at
   emit time.
4. Main thread collects via `ReorderBuffer<Vec<OwnedAction>>`, runs
   the original emit callback per action for stdout + stats.

**Risks.**
- Owned actions inflate memory if tasks get too far ahead; bound the
  worker queue to ~2 × N_workers to cap peak in-flight.
- Pre-pass must handle the stash/continue logic for non-OsmData
  blobs in the file (header blobs, etc.) correctly.
- Residual semantics subtle: the "last task in a type-phase" case
  needs to drain the trailing single-sided blobs.

**Expected impact.** See ceiling table. Germany 103 s → ~15-21 s on
6-8 cores. Planet 2134 s → ~336-444 s (5.6-6m to 7m24s).

### 2. Parallel decode prefetch (sub-component of item 1, not a standalone win)

Previously listed as the headline item before the hotpath data arrived.
Demoted because decode is only 29.5 % of the wall - parallel decode in
isolation caps at ~1.25 × wall speedup, missing the target by a wide
margin.

Still a component of item 1: the worker's per-task decode runs on a
worker thread (off the consumer critical path). The implementation
pattern that was sketched here (thread-local scratch, rayon pool,
reorder buffer) is reused inside item 1's worker shape.

Standalone plan shape below kept for reference; do not land it by
itself.

<details>
<summary>Previous standalone plan (click to expand, kept for context)</summary>

Reader threads feed `sync_channel<PendingBlob>` (depth 8 per side).
Consumer runs the existing merge control flow, but the synchronous
`decode_pending` calls are replaced by "submit to rayon + wait on
front of `VecDeque<oneshot<BlockState>>`". Preserves residual handling,
preserves v1 byte-equal gate, ~64 MB peak in-flight memory, auto
reorder.

Why standalone is insufficient: the consumer thread still runs
`element_merge_pair` sequentially and that is where 67.7 % of the
wall lives. Parallel decode feeds a bottlenecked consumer.

</details>

### 3. Reduce per-element cost in `element_merge_pair`

**Secondary axis.** Even with fully parallel blob-pair merge, each
worker still runs the same per-element inner loop. Making that loop
cheaper multiplies the parallel win.

Candidates, in descending impact estimate:

**3a. Monomorphise `on_action` via generics.** Today
`block_pair_merge_phase` and `element_merge_pair` take
`on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> BoxResult<()>`.
Every element in the 2 × 10⁹ loop makes an indirect call. Switch to
a generic parameter `F: FnMut(BlockMergeAction<'_>) -> BoxResult<()>`
and the compiler inlines. Expected: ~1-2 ns saved per element × 2 B
elements = 2-4 s. Easy change, monomorphisation bloat is bounded
(three callers: diff, derive_changes, possibly a test).

**3b. Avoid the `Result<(), BoxError>` propagation per element.** The
`?` on every `on_action` return is rarely a real error path (output
IO errors are rare and terminal). Consider collecting into a
`Vec<Action>` and returning at the end of each blob pair, or passing
a `&mut ActionSink` with non-failing `push` methods. Some per-element
overhead saved; hard to quantify without a microbench.

**3c. Precompute a "tags changed" signal in indexdata.** For each
blob, store a stable hash of (sorted tag pairs). When two overlapping
blobs have the same tag-hash AND same id/lat/lon content, the whole
block can be marked `Equal` without the per-element tag iter compare.
Format change - requires `add-locations-to-ways` / writer cooperation;
benefit depends on how many blobs have identical tag content in
practice. Defer until items 1 and 3a are measured.

**3d. SIMD over the packed Sint64 id/lat/lon arrays.** The element
iterator decodes three parallel varint streams per dense node. If
the two sides can be compared stream-to-stream before walking tags,
a SIMD varint-compare could filter out "definitely equal" runs in
bulk. Speculative - depends on how much of the 35 ns/element is in
varint decode vs tag scan.

Expected aggregate impact of 3a + 3b: ~10-15 % wall reduction on
top of item 1. Items 3c and 3d are further-out research.

### 4. ~~v3 - skip decode for entirely single-sided blobs~~ CLOSED 2026-04-19

Shadow counters on `a9d430f2`:
- `blobs_old_only = 0`
- `blobs_new_only = 440` (out of ~300 K blob pairs ≈ 0.15 %)

Less than the 0.4 % analytical estimate. Skipping these saves at most
0.15 % of the wall - not worth the new `BlobOldOnlyByIndex` /
`BlobNewOnlyByIndex` variants, the opt-in callsite changes, or the
user-visible behavior split on `diff` vs `derive_changes`. Kept here
as a dead item so future reviewers don't re-propose it.

### 5. ~~Overlap BlobEqual checks could short-circuit earlier~~ CLOSED 2026-04-19

Shadow counters on `a9d430f2`:
- `pairs_byte_equal = 0` across the full run.

Two independent PBFs produced by different toolchains never share
byte-identical blobs even when their element content is identical.
The v1 byte-equal fast path is a `diff`-on-apply-changes optimization
and has no signal for `diff-snapshots`. No work to sharpen here.

### 6. Element-merge allocation audit

**Observation.** `diff_block_pair` uses borrowed elements and is
documented as zero-String-alloc for the `Equal` path (98.8 % of
elements). Hotpath confirms: minor faults 328 K over 2107 s at
planet are mostly page dirtying during decoded block construction.
At 300 K blob pairs, ~1 fault per blob - about what
`PrimitiveBlock::from_vec_with_scratch` costs. Not a reducible item.

### 7. Direct-IO / io_uring read path

**Observation.** Reader uses buffered `FileReader` (256 KB `BufReader`
with `fadvise(SEQUENTIAL)`). Disk bandwidth is not the bottleneck at
~85 MB/s observed vs 2+ GB/s available. Switching I/O primitives
gives nothing on its own.

**Composition with item 1.** If parallel blob-pair merge lifts
throughput enough that I/O becomes the ceiling, then O_DIRECT or
io_uring matters. Not before. Closed for now.

## Ordering

1. ~~**Shadow counters + markers + hotpath annotations**~~ - landed
   2026-04-19 (`052da8b`).
2. ~~**Run a measured `diff-snapshots` at planet.**~~ - landed
   2026-04-19 as `a9d430f2`. Closes v1 and v3 optimisation items.
3. ~~**Hotpath inner-function annotations + CLI destructor fix.**~~ -
   landed 2026-04-20 (`0b92a8f` annotations, `a3795c2` ExitCode).
   First clean hotpath run: germany `e45f81c4`. Resolved the "where
   is the wall" question: 67.7 % in `element_merge_pair`, 29.5 % in
   `decode_pending`, meaning decode-only parallelism is insufficient.
4. ~~**Germany iteration dataset.**~~ - `snapshot.20260420` registered
   `e27c89e`. Baseline `22e4f65f` at 103.3 s. Use this instead of
   planet during the inner loop.
5. ~~**Parallel blob-pair merge (item 1 above).**~~ landed 2026-04-20.
   Shard-based, CLI `-j/--jobs N` on `pbfhogg diff`.
6. ~~**Pread-only walker.**~~ landed 2026-04-20. Planet walker phase
   **32.9 s → 14.9 s** (45 GB → 2.6 GB read); now syscall-bound on
   ~600 K preads at planet scale.
7. **Follow-ups (open).**
   - **Generalise to `derive_changes` / `--format osc`.** The
     parallel path today only implements `diff`. The OSC emit
     logic (`<create>/<modify>/<delete>` XML) can follow the
     same shard plan; the worker writes owned XML fragments
     that main concatenates in shard order. CLI currently
     rejects `-j > 1` when `--format osc`.
   - **Auto-enable by default.** Evaluate flipping the default
     `-j` from `1` to `0` (auto from `available_parallelism()`)
     once we have experience with the parallel path in the wild.
   - **Halve walker syscalls.** One pread of (length_prefix +
     header) per blob instead of two separate preads - reads 1 KB
     (covers any reasonable header) at the blob offset, parses
     length + header from the buffer. Cuts ~300 K syscalls at
     planet, saving ~7 s.
7. **Per-element cost reductions (item 3a, 3b from the ranked list).**
   Only if workloads with smaller blob counts (where the walker is
   proportionally larger, or the per-shard merge becomes very
   short) show headroom.
8. **Research follow-ups (item 3c, 3d; 6; 7).** Only if we hit a
   new wall that item 6/7 can't reach.

## Measured outcome (2026-04-20, shard-based parallel merge)

Commit landing series ending at `a3795c2` (cli ExitCode fix) with
`src/commands/diff/parallel.rs` as the new path.

| Dataset | -j     | Wall        | Speedup vs baseline | Utilization (NODE) |
|---------|-------:|-------------|---------------------|--------------------|
| Germany |    1   | 103.3 s     | 1.0× (baseline)     | `avg_cores=1.0`    |
| Germany |    4   |  34.8 s     | 3.0×                | `avg_cores=3.6`    |
| Germany |    8   |  17.0 s     | 6.1×                | `avg_cores=6.9`    |
| Germany |    8*  |  18.9 s     | 5.5×                | `avg_cores=6.7`    |
| Planet  |    1   | 2134.3 s    | 1.0× (baseline)     | `avg_cores=1.0`    |
| Planet  |   16   |  234.7 s    |  9.1×               | `avg_cores=14.7`   |
| Planet  |   16*  |  219.4 s    | **9.7×**            | `avg_cores=14.6`   |

`*` = pread walker (later in the session, commit TBD).

Planet `avg_cores=14.7` out of 16 shards in the NODE phase (92 %
utilization). Peak RSS 2.4 GB (comfortable on 27 GB hosts). Walker
phase is 32.9 s of the planet run (14 % of wall) on a cold cache,
drops to 0.5 s when cached.

The result beats the "Both parallelised (8 cores, ideal)" ceiling
row in the Amdahl table above (336 s predicted vs 235 s measured),
because plantasjen has more than 8 cores and the 16-shard dispatch
used them.

## Constraints

- `derive_changes` BlobNewOnly emits full `<create>` XML per element -
  cannot skip decode. Any v3 work only helps BlobOldOnly for this
  command. `diff` human-readable path has more flexibility.
- Both callers require indexed inputs for the block-pair fast path.
  The element-stream fallback (`diff_element_stream`,
  `derive_changes_element_stream`) is already owned-allocation-heavy
  and not the planet baseline. Parallel decode applies there too
  via the pipelined reader, but priorities are the indexed path.
