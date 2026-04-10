# parallel_classify_phase regression brief — 2026-04-10

**Audience:** `planet` + `perf` + `arch` (and possibly `bugs`) review archetypes
**Purpose:** Resolve a measured post-refactor regression before implementing a fix.

> **Post-review note (2026-04-10):** This brief was sent to `review planet,perf,arch` and four reviewers responded. Their full responses are at [`notes/parallel-classify-regression-2026-04-10-reviews.md`](parallel-classify-regression-2026-04-10-reviews.md). The brief has two errors the reviewers caught:
> 1. The "structurally identical" framing of `extract.rs:2813` and `tags_filter.rs:1000` is too strong — the call sites are byte-identical at the API level but not workload-equivalent (smart-extract uses a relation-driven `extra_way_ids`, tags-filter uses a tag-selective `included_way_ids`; chunk spread differs ~5×).
> 2. The Q3 framing of "shared accumulate-mode wall regression" is contradicted by the brief's own measurement table: tags-filter PASS1 (the +32% outlier) uses `parallel_classify_phase`, not `parallel_classify_accumulate`. The wall regression has multiple causes, not one. See `notes/columnar-integration.md` "Open: cross-command wall regression" for the deferred investigation plan.
>
> The brief is otherwise left as a snapshot of the pre-review state. The corrected analysis is in [`notes/columnar-integration.md`](columnar-integration.md) (resolved section).

## TL;DR for reviewers

Yesterday (2026-04-09) you signed off on per-worker accumulation for `parallel_classify_phase`. Today's HEAD measurements at Europe scale show:

- **`extract --strategy smart`**: peak anon **4.71 GB → 10.72 GB** (+128%), wall **208s → 254s** (+22%), PASS3 major faults **59 → 2,861** (48×).
- **`tags-filter-twopass`**: peak anon **2.01 GB → 2.06 GB** (FLAT), wall **105s → 130.5s** (+24%).

The two phenomena are distinct:
1. **One path-specific memory regression**, almost certainly `extract.rs:2813` (smart-extract way-dep `IdSetDense` accumulation — explicitly listed as "disputed" in `notes/columnar-integration.md`).
2. **One uniform wall-time regression** that affects both paths and is NOT explained by memory on tags-filter. Same magnitude on both. Mystery.

We need your help resolving three questions before we change code:

- **Q1** Is `extract.rs:2813` actually the cause of the memory regression in extract-smart, or could it be something else?
- **Q2** Why does the chunk-spread model predict ~9 GB per IdSetDense path but the measured tags-filter peak is flat at 2 GB? (Either the model is wrong, or the path isn't actually accumulating the way the model assumed.)
- **Q3** What shared cause explains the +22-24% wall regression on BOTH paths, given that only one has a memory issue?

You have full repo access — file references throughout this doc are clickable starting points, not summaries.

---

## Background and how we got here

pbfhogg shipped v0.1 to crates.io recently. Post-release, we're working through `TODO.md` items. The "parallel_classify_phase planet safety" item references `notes/columnar-integration.md` (please skim it — the design review is lines 110–156).

The summary of that design review (2026-04-09, 10 reviewers, 5 archetypes):

- The single-parameter "per-worker accumulate" `parallel_classify_phase<S>` shipped as part of the columnar-decode work to reduce single-extract Japan alloc from 6.4 GB to 2.0 GB.
- At planet scale, several call sites would have unbounded per-worker `S` if they accumulated `Vec<i64>` results or `IdSetDense` whose chunk spread covers the full node ID range.
- The recommendation was to restore a two-parameter `parallel_classify_phase<S, R>` (per-blob send) for the dense paths, keep accumulate only for sparse/bounded paths (relation classify, relation-member closures).
- Three paths were marked **disputed** — way-dep `IdSetDense` accumulation in tags-filter, smart extract, and the geocode builder. The dispute was whether 6 workers × 1.5 GB = 9 GB fits on a 30 GB host alongside other allocations.
- The design review explicitly said: "Decision pending. If we keep it, need planet-scale measurement to confirm 9 GB fits. If not, revert to per-blob send."

**Current code state** (verified 2026-04-10 by reading `src/commands/mod.rs:478` and `:561`):
- `parallel_classify_phase<S, R>` (per-blob send): **already exists** with the recommended two-parameter signature. Doc comment says "Use for dense/hot paths." Lines 478–551.
- `parallel_classify_accumulate<S>` (per-worker accumulate): also exists. Doc comment says "Use ONLY for sparse paths where per-worker `S` is bounded at planet scale." Lines 561–633.

So **the API split is in place**. The question is whether the call sites pick the right function. We audited all call sites:

| File:line | Path | Per-worker `S` | Should be? | Currently? |
|---|---|---|---|---|
| `extract.rs:761` | simple-extract node bbox classify | (uses `_phase`, not accumulate) | `_phase` | `_phase` ✓ |
| `extract.rs:778` | simple-extract way classify | (uses `_phase`) | `_phase` | `_phase` ✓ |
| `extract.rs:863` | simple-extract pass | `_phase` | `_phase` | `_phase` ✓ |
| `extract.rs:922` | multi-extract Phase 3 — relation classify | `Vec<IdSetDense>` per region | `accumulate` (bounded) | `accumulate` ✓ |
| `extract.rs:2254` | simple-extract Phase 3 — relation classify | `IdSetDense` (bounded) | `accumulate` | `accumulate` ✓ |
| `extract.rs:2553` | complete/smart Phase 1 (nodes) | `_phase` | `_phase` | `_phase` ✓ |
| `extract.rs:2591` | complete/smart Phase 2 (ways) | `_phase` | `_phase` | `_phase` ✓ |
| `extract.rs:2616` | complete/smart Phase 3 (relations) | `(IdSetDense, IdSetDense, IdSetDense)` (bounded by relations) | `accumulate` | `accumulate` ✓ |
| **`extract.rs:2813`** | **smart-extract PASS2 way-dep node collection** | **`IdSetDense` (unbounded — chunks spread across full node ID range)** | **`_phase` (per-blob `Vec<i64>`)** | **`accumulate`** ⚠ |
| `tags_filter.rs:596` | tags-filter pass 1 | `_phase` | `_phase` | `_phase` ✓ |
| `tags_filter.rs:931` | `collect_relation_member_closure` | `ClosureResult { Vec, Vec, Vec }` (bounded by relation member count) | `accumulate` | `accumulate` ✓ |
| **`tags_filter.rs:1000`** | **`collect_way_node_dependencies`** | **`IdSetDense` (unbounded — same shape as extract.rs:2813)** | **`_phase` (per-blob `Vec<i64>`)** | **`accumulate`** ⚠ |
| `getid.rs:149` | getid pass 1 | `_phase` | `_phase` | `_phase` ✓ |
| `getid.rs:413` | getid `--add-referenced` | (not measured) | likely `_phase` | `accumulate` (TBD) |

**Two suspect call sites**: `extract.rs:2813` and `tags_filter.rs:1000`. Both are way-dep `IdSetDense` accumulation. Both are exactly the "disputed" entries from the design review.

---

## Measurements (commit `5ca2df9` aka HEAD, host: plantasjen, 30 GB RAM)

All measurements are `--bench 1` (single iteration, sidecar profiler attached). Benches are gated against parallel execution.

### Europe extract-smart

**Pre-refactor `fc17b51` (2026-03-30, UUID `f420c5fd`):**
```
Phase                      Duration   Peak RSS  Peak Anon  Peak Mflt
EXTRACT_PASS1              60636ms 4113852kB 4108760kB          0
EXTRACT_PASS2              20579ms 4129588kB 4124496kB          0
EXTRACT_PASS3             126710ms 4713328kB 4709368kB         59
Wall total: 208s
Overall p95 anon: 4.63 GB
```

**HEAD `5ca2df9` (2026-04-10, UUID `01de22bb`):**
```
Phase                      Duration   Peak RSS  Peak Anon  Peak Mflt
EXTRACT_PASS1              58408ms 3704836kB 3699720kB         19
EXTRACT_PASS2              33776ms 10719508kB 10716320kB          5
EXTRACT_PASS3             147975ms 8397544kB 8395620kB       2861
Wall total: 254s
Overall p95 anon: 8.40 GB
```

**Deltas:**
- PASS1: -10% anon (3.70 vs 4.11 GB), -4% wall — slightly improved
- **PASS2: +160% anon (10.72 vs 4.12 GB), +64% wall (33.8 vs 20.6s)** — huge regression
- PASS3: +78% anon (8.40 vs 4.71 GB), +17% wall, **+48× major faults**
- Wall total: +22% (254 vs 208s)
- Peak anon: +128% (10.72 vs 4.71 GB)

PASS2 in `complete/smart` extract is implemented at `src/commands/extract.rs:2801–2833` (read it — it's the smart-strategy "extra way dep" pass that resolves nodes referenced by ways pulled in via relation members). It's the only `accumulate` call in the smart path that uses an unbounded `IdSetDense`. The 10.7 GB peak corresponds to 6 workers × ~1.5–1.8 GB IdSetDense each (chunk spread covers the full Europe node ID range).

**Pro-rated to planet (~2.6× Europe): peak ~28 GB anon. Does not fit 30 GB host alongside kernel + page cache.** Planet blocker confirmed.

### Europe tags-filter-twopass

**Pre-refactor `75ad21d` (2026-03-30, UUID `59361b65`):**
```
Phase                      Duration   Peak RSS  Peak Anon  Peak Mflt
TAGSFILTER_PASS1           34324ms 1874132kB 1869284kB          0
TAGSFILTER_PASS2           37381ms 2019892kB 2014808kB          0
Wall total: 105s
Overall p95 anon: ~1.97 GB
```

(Pre-refactor sidecar didn't have CLOSURE/WAYDEPS as named markers — they're new in HEAD.)

**HEAD `5ca2df9` (2026-04-10, UUID `c1672f04`):**
```
Phase                      Duration   Peak RSS  Peak Anon  Peak Mflt
TAGSFILTER_PASS1           45291ms   70368kB   66016kB          1
TAGSFILTER_CLOSURE         16079ms 1869028kB 1864676kB          0
TAGSFILTER_WAYDEPS         19822ms 1896932kB 1892388kB          0
TAGSFILTER_PASS2           38505ms 1981160kB 1976368kB          0
Wall total: 130.5s
Overall p95 anon: 1.99 GB
```

**Deltas:**
- PASS1 anon: **96% reduction** (66 MB vs 1.87 GB) — accumulate is doing what it was supposed to here
- PASS1 wall: +32% (45.3 vs 34.3s)
- CLOSURE / WAYDEPS — new phases, ~1.86 GB and ~1.89 GB respectively
- PASS2: flat (1.98 vs 2.01 GB)
- **Wall total: +24%** (130.5 vs 105s)
- **Peak anon: FLAT** (2.06 vs 2.01 GB)

The 1.86–1.89 GB "peaks" during CLOSURE and WAYDEPS phases are dominated by the persistent `IdSetDense` state built up during PASS1 (`included_way_ids`, `included_node_ids`, `relation_dep_node_ids`, etc.). Europe-scale `IdSetDense` is ~460 MB per instance × 3–4 sets = ~1.5–1.8 GB persistent. The per-worker `accumulate` IdSetDense at `tags_filter.rs:1000` is contributing on top of that, but the peak is essentially the persistent state, not the worker scratch.

**Wait — that doesn't match the design review's prediction.** The design review said way-dep `IdSetDense` accumulation should produce 6 workers × ~1.5 GB = 9 GB per-worker memory at planet scale. We measured tags-filter total peak at 2 GB on Europe (~1/3 of planet). Pro-rate to planet: ~5.5 GB. **Way under the 9 GB the model predicted.**

This is **the** interesting question. Either:
- The chunk-spread argument is wrong, and per-worker `IdSetDense` doesn't actually allocate ~1.5 GB per worker because work-stealing keeps each worker's chunk touch pattern narrower than expected.
- The merge-into-shared happens incrementally per worker as workers complete, not all-at-once at the end, so we never actually have N copies live simultaneously.
- The peak is being attributed to the persistent state and the per-worker contribution is hidden under it.
- Something else we haven't thought of.

If the chunk-spread argument is wrong, then **`extract.rs:2813`'s 10.7 GB peak isn't actually from per-worker IdSetDense at all** — it's from something else, and we'd be fixing the wrong thing.

---

## The three questions

### Q1 — Is `extract.rs:2813` the actual cause of the extract-smart PASS2 memory regression?

The path is well-aligned with the prediction: smart extract PASS2 jumped from 4.12 GB to 10.72 GB (+6.6 GB), the path uses per-worker `IdSetDense` accumulation, and the design review explicitly flagged this exact site as "disputed: 6 workers × 1.5 GB = 9 GB."

But (per the tags-filter measurement) the chunk-spread model that predicts 9 GB doesn't match the tags-filter reality. So either:
- (a) Extract-smart hits a different access pattern that genuinely causes the 9 GB blowup, while tags-filter does not. Plausibly because smart-PASS2 ways scan the FULL way blob set with very poor locality (way refs span the entire node ID range — chunks scatter randomly), while tags-filter's `collect_way_node_dependencies` only scans ways already in `included_way_ids`, a much smaller and more spatially-clustered subset.
- (b) The 6.6 GB extra in extract-smart PASS2 is coming from somewhere else entirely — maybe BlockBuilder allocation in the smart relation pass running concurrently, maybe a forgotten Vec growth, maybe something in the columnar `DenseNodeColumns` worker scratch that wasn't there in pre-refactor.

We need (a) confirmed or (b) found before changing code. **What would you look at first?** Hotpath profile? Allocation tracking? A different sidecar marker? Re-running with `RUSTFLAGS=-Cforce-frame-pointers=yes` and a perf flamegraph?

### Q2 — Why is the chunk-spread model wrong for tags-filter but maybe right for extract-smart?

If `tags_filter.rs:1000` and `extract.rs:2813` are structurally identical (both accumulate per-worker `IdSetDense` of way refs), why does only one of them produce a measurable per-worker blowup?

Hypotheses to evaluate:
- **Selectivity.** tags-filter only scans ways in `included_way_ids` (a small set). Smart-extract PASS2 scans ways in `extra_way_ids` (potentially the full extra-way set from relation member resolution, which can be huge for boundary/multipolygon-heavy regions). Different "match density" → different chunk-touch patterns → different per-worker memory.
- **Workload size.** tags-filter scan is bounded; smart PASS2 may be unbounded relative to total way count.
- **Worker stealing pattern.** rayon (or our manual scheduler) may distribute tags-filter work differently than smart PASS2 work because of the schedule shape.

If the answer is "they're not actually structurally equivalent and only extract-smart hits the worst case," then the fix is small and surgical: convert just `extract.rs:2813` to `parallel_classify_phase<DenseScratch, Vec<i64>>` per-blob send. Leave `tags_filter.rs:1000` on accumulate (it's working).

If the answer is "they should both be problematic but tags-filter is somehow getting away with it via incremental merge or something subtle," we need to understand why before deciding whether to leave `tags_filter.rs:1000` alone or also flip it.

### Q3 — What explains the +22-24% wall-time regression on BOTH paths?

This is the question I'm least equipped to answer. Both extract-smart (+22%) and tags-filter-twopass (+24%) regressed in wall time by nearly identical magnitudes. **Tags-filter has flat memory**, so the wall regression on tags-filter cannot be page-cache thrashing. There has to be a shared cause.

Candidates:
- **Allocator pressure** from many small `Vec`s churning in per-worker accumulate state. Wouldn't show up in peak anon because each individual Vec is small, but could slow steady-state.
- **Worker init/teardown.** Per-worker `S` setup/tear-down may be heavier than per-blob send for moderate-work-per-worker scenarios.
- **Reduced effective parallelism.** With per-worker accumulate, all workers must finish before merge runs. With per-blob send, merge runs concurrently with later worker activity. The wall difference could be the lost overlap.
- **Mutex contention on the schedule receiver.** Both `parallel_classify_phase` and `parallel_classify_accumulate` use `Arc<Mutex<Receiver>>` for the schedule descriptor channel. With more work happening per worker (because they accumulate), workers may hold the mutex longer or contend more.
- **Build-level changes between `75ad21d`/`fc17b51` and `5ca2df9`.** These are ~10 days apart. Other commits may have introduced unrelated regressions that affect both paths.
- **Measurement noise.** Single-run benches at Europe scale have ~5–10% noise. 24% is bigger, but not enormously so. Could 1 run give us a misleading reading?

The tags-filter result is the strong evidence that this is NOT (only) a memory issue. **What's the fastest way to isolate the shared cause?** Profile both with hotpath at the parallel_classify level? Re-run with `--bench 3` for noise reduction first? Strip the columnar work and see if the regression survives?

---

## What we plan to do based on your answers

**If Q1 confirms `extract.rs:2813` is the cause:** convert that one site to `parallel_classify_phase<S, R>` with `S = some scratch` (probably nothing — the closure is simple) and `R = Vec<i64>` of way node refs per blob. Merge calls `extra_node_ids.set(id)` per id, just like the existing closure drains do. Self-contained change, ~50 LoC.

**If Q1 finds something else:** investigate that instead. Could be a different site, could be a buffer growth bug, could be a regression elsewhere.

**If Q2 confirms tags-filter's working state is "real":** leave `tags_filter.rs:1000` on accumulate. Update `notes/columnar-integration.md` to note the chunk-spread model is too pessimistic for selective workloads.

**If Q2 says tags-filter is also broken but hidden:** convert it as well, with the same shape.

**If Q3 isolates the wall regression:** address it as a separate fix. May or may not depend on Q1/Q2.

**If Q3 is "just measurement noise":** rerun with `--bench 3` for confirmation, then move on.

---

## Files the reviewers will probably want to read

- `notes/columnar-integration.md` — original design review (please re-read lines 110–156, the planet-safety section)
- `src/commands/mod.rs:421-633` — both `parallel_classify_phase<S, R>` and `parallel_classify_accumulate<S>` definitions, with doc comments
- `src/commands/extract.rs:2801-2833` — the extract-smart PASS2 way-dep call site (the suspect)
- `src/commands/extract.rs:2553-2647` — `collect_pass1_generic` for context on the smart-extract pass structure
- `src/commands/tags_filter.rs:987-1024` — `collect_way_node_dependencies` (the structurally-identical site that's NOT regressing visibly)
- `src/commands/tags_filter.rs:902-985` — `collect_relation_member_closure` (correctly using accumulate, bounded)
- `src/commands/id_set_dense.rs` — the chunk-allocation behavior the planet-safety argument is based on
- `.brokkr/results.db` — full sidecar trajectories for the four UUIDs cited above (`f420c5fd`, `01de22bb`, `59361b65`, `c1672f04`); query with `brokkr results <uuid> --timeline --fields t,rss,anon,majflt --every 50` for full per-100ms samples
