# Columnar decode integration research

## Current state

Prototype shipped (commit `e0b0780`). `DenseNodeColumns` in
`src/read/columnar.rs` batch-decodes IDs, lats, lons into contiguous
arrays. Two classification methods:
- `collect_matching_ids_bbox` — single-region bbox, output to Vec
- `collect_matching_ids_multi_bbox` — N-region bbox, output to Vec

Used in:
- Single-extract node classification for bbox regions (Vec path)
- Multi-extract node classification for all-bbox regions (Vec path)

The IdSetDense output methods (`set_matching_ids_bbox`,
`set_matching_ids_multi_bbox`) were removed in commit `c4c7b9e` after
confirming they were unused in production. Direct IdSetDense::set() in
tight loops caused 29x regression vs Vec::push() due to random chunk
access. See "IdSetDense accumulation findings" below.

ASM inspection confirms LLVM does NOT autovectorize the bbox loop —
the `push()` side effect prevents it. Explicit AVX2 intrinsics are
the only path. The multi-bbox loop is a better SIMD target: N region
tests per node amortizes setup.

## Measured results (commit `c3b271f`, plantasjen)

### Multi-extract Japan 5-region node classify phase

| Metric | Baseline (element-by-element) | +columnar | Final |
|--------|------------------------------|-----------|-------|
| Node classify | 1081ms | 748ms (-31%) | ~750-925ms |
| Total | 8.1s | 7.3s | ~7.3s |

### Single-extract Japan alloc

| Metric | Baseline (commit `ec43a8b`) | Final (commit `201a4cf`) |
|--------|----------------------------|--------------------------|
| Total alloc | 6.4 GB | 2.0 GB (-69%) |
| `parallel_classify_phase` | 5.0 GB (48.8%) | Not in top 10 |

The single-extract alloc reduction comes from per-worker accumulation
in `parallel_classify_phase` — workers accumulate Vec<i64> across all
blobs and send once at completion. No per-blob allocation through the
channel.

### parallel_classify_phase refactor

Workers now own persistent state `S` via `worker_init`. The classify
closure mutates `S` without returning per-blob results. Merge receives
the final `S` once per worker at scope exit.

For hot paths (node/way classify): S = Vec or Vec<Vec> — sequential
push, cache-friendly, consumer iterates into IdSetDense.

For sparse paths (relation classify, way dep scans, geocode): S =
IdSetDense — direct set() is fine since filter work dominates and
match counts are low. Merge uses `IdSetDense::merge` (bitwise OR,
zero-copy for non-overlapping chunks).

## IdSetDense accumulation findings (commit `e94c3c8`)

Direct `IdSetDense::set()` in tight classify loops:
- **Alloc: 20.5 MB** (down from 8.7 GB) — 99.8% reduction
- **Node classify: 20.5s** (up from 713ms) — 29x regression
- **Way classify: 3.7s** (up from 943ms) — 4x regression

Root cause: `IdSetDense::set()` does chunk lookup + byte offset +
bitmask per ID (random access). `Vec::push()` is sequential append
(L1 cache-friendly). IDs are already sorted from the columnar decode,
so the randomness is in the chunk access pattern, not ID order.

Perf reviewer consensus (2 reviewers):
- Hybrid is correct: Vec push in hot loop, drain to IdSetDense
- Direct set() fine for sparse paths (polygon, relation, way deps)
- Per-worker IdSetDense memory acceptable with sparse chunk allocation
- A batch `set_sorted_batch()` could amortize chunk lookup but still
  slower than push() due to read-modify-write on potentially cold lines

## Integration opportunities

### 1. Multi-extract node classification — DONE

`collect_matching_ids_multi_bbox` tests each node against all N
bboxes in one pass over contiguous i32 lat/lon arrays. Per-worker
`DenseNodeColumns` + `Vec<Vec<i64>>` reused across blobs. Polygon
regions fall back to element-by-element.

### 2-5. ALTW, external join, geocode, node stats — NOT GOOD TARGETS

See previous analysis (unchanged). Wire-format scanners and tag-based
filtering make columnar inapplicable.

## Columnar for ways and relations

Conclusion unchanged: columnar is primarily valuable for dense nodes.
Ways and relations are better served by element-by-element iteration
or wire-format scanners.

## Relationship to SIMD

Columnar arrays are the prerequisite for SIMD. The multi-bbox loop
is the best SIMD target (N region tests per node, amortized setup).

SIMD becomes worthwhile when:
- Multi-region classification (NOW IN PLACE)
- Polygon PIP (expensive per-node computation)
- Batch varint decode in protohoggr (different target, broader impact)

## Next steps: planet-safe parallel_classify_phase

Full design review (10 reviewers, 5 archetypes, 2026-04-09) concluded
that per-worker Vec accumulation is NOT planet-safe for dense paths.
The per-blob send pattern (v1) is the correct permanent solution.

### Consensus findings

**Per-blob send is planet-safe.** Each per-blob Vec bounded by blob
size (~64 KB per region). Consumer throughput is not a bottleneck for
realistic workloads (continental extract: ~26s consumer vs ~33s decode
at planet scale). Only pathological for whole-planet identity extract.

**API: restore two-type-parameter signature.** `S` for persistent
scratch (DenseNodeColumns, etc.), `R` for per-blob results (Vec<i64>).
Workers send `R` per blob, keep `S` across blobs. Two functions:
- `parallel_classify_phase<S, R>` — per-blob sends with scratch
- `parallel_classify_accumulate<S>` — per-worker accumulation (current)

### Per-path planet memory analysis

| Path | State | Planet per-worker | Planet-safe? | Action |
|------|-------|-------------------|-------------|--------|
| Node classify (multi, 5 regions) | Vec<Vec<i64>> | 3.5 GB | **No** | Per-blob send |
| Node classify (single) | Vec<i64> | 700 MB | Marginal | Per-blob send |
| Way classify (single) | (Vec<i64>, Vec<i64>) | 1.6 GB way + 9.5 GB refs | **No** | Per-blob send |
| Way classify (multi, 5 regions) | Vec<Vec<i64>> | 8 GB | **No** | Per-blob send |
| tags_filter pass 1 ClassifyResult | Vec<(i64, Vec<i64>)> | 2.9 GB | **No** | Per-blob send |
| Relation classify | (IdSetDense×3) | 68 MB | **Yes** | Keep accumulate |
| tags_filter relation closure | Vec<i64> members | 13 MB | **Yes** | Keep accumulate |
| Way dep node refs (tags_filter, `tags_filter.rs:1000`) | IdSetDense | ~200 MB (measured at Europe, see below) | **Yes** (workload-selective) | Keep accumulate |
| Way dep node refs (smart extract, `extract.rs:2813`) | IdSetDense | 1.5 GB+ (measured at Europe, see below) | **No** | Per-blob send |
| Geocode referenced nodes | IdSetDense | 1.5 GB (estimated, not measured) | Likely No, untested | Measure before deciding |

### Resolved: way dep IdSetDense accumulation (2026-04-10 measurement)

The "disputed" framing from the 2026-04-09 design review was based on a
**worst-case** chunk-spread model (6 workers × full node ID range = 9 GB
per-worker). Measurement at Europe scale on 2026-04-10 (commit `5ca2df9`,
plantasjen, sidecar profiler) showed the model is correct for the worst
case but **too pessimistic for selective workloads**. The decision is
not all-or-nothing — it's per-call-site, driven by what produces the
input way ID set.

**Measured:**
- `extract --strategy smart` Europe: EXTRACT_PASS2 peak anon **10.72 GB**
  (UUID `01de22bb`). Pre-refactor `fc17b51` was 4.12 GB. The +6.6 GB
  matches 6 workers × ~1.1 GB per-worker IdSetDense exactly. Pro-rated
  to planet (~2.6×): ~28 GB. Does NOT fit on 30 GB host. **Confirmed
  planet blocker.**
- `tags-filter-twopass` Europe: TAGSFILTER_WAYDEPS peak anon **1.89 GB**
  (UUID `c1672f04`), but ~1.7 GB of that is the persistent IdSetDense
  state from prior phases (`included_way_ids`, `included_node_ids`,
  etc.). Per-worker contribution is ~200 MB total. Pro-rated to planet:
  ~5.5 GB. **Comfortably fits 30 GB host.**

**Why the difference?** The two call sites are byte-identical at the API
level but **not workload-equivalent**:
- `tags_filter.rs:1000` `collect_way_node_dependencies` filters on
  `included_way_ids` — a **tag-selective** subset (e.g. `highway=primary`
  ≈ 0.1% of all ways). Tag selectivity correlates with geography
  (highways cluster along road networks), so per-worker chunk spread is
  narrow (~50–200 chunks per worker, ~200–800 MB).
- `extract.rs:2813` smart PASS2 filters on `extra_way_ids` — a
  **relation-driven** subset of ways pulled in via relation member
  expansion (coastlines, admin boundaries, multipolygons). Relations
  span continents, so the extra-way set is wide and globally dispersed.
  Per-worker chunk spread approaches the worst case (~500 chunks,
  ~1.5+ GB).

**The chunk-spread model is workload-dependent, not a uniform property
of `parallel_classify_accumulate`.** The same helper, the same data
structure, applied to two filtered subsets of the same way blob set,
produces 5× different per-worker memory. The model is correct as a
worst-case prediction; it's wrong as a uniform prediction across all
"way deps" call sites.

**Per-call-site decisions:**
- `extract.rs:2813` (smart PASS2 way deps): **Per-blob send**.
  Convert to `parallel_classify_phase<(), Vec<i64>>`. The relation-driven
  workload genuinely hits the worst case the model predicted.
- `tags_filter.rs:1000` (tag-selective way deps): **Keep accumulate**.
  Add a comment at the call site documenting that this is safe ONLY
  because `included_way_ids` is tag-selective (narrow + clustered).
  Future maintainers should NOT copy this pattern for relation-driven
  filters.
- Geocode referenced nodes: **Not yet measured**. The path is
  `geocode_index/builder.rs` Pass 1.5 (referenced node collection),
  built from relation members + way refs. Has both selective and
  dispersed characteristics depending on which relation/way categories
  it includes. Measure on Europe before deciding.

**Defensive guard (suggested by planet/claude):** consider adding a
runtime heuristic that falls back to per-blob send if the input
filter set has high chunk spread (e.g. `extra_way_ids.chunk_count() >
threshold`). This future-proofs against new callers hitting the same
trap as smart PASS2. Not yet implemented; revisit after the
extract.rs:2813 fix lands.

### Open: cross-command wall regression (separate issue)

The 2026-04-10 measurement also showed a **+22-24% wall-time regression**
on both `extract --strategy smart` (208s → 254s) and `tags-filter-twopass`
(105s → 130.5s) at Europe scale. This is **not** explained by the memory
issue above:
- Tags-filter has flat memory and still regressed +24% wall.
- Tags-filter PASS1 — the phase with the largest wall regression
  (+32%) — already uses `parallel_classify_phase`, NOT
  `parallel_classify_accumulate`. So accumulate-mode semantics cannot
  fully explain the tags-filter wall regression.

Multiple causes likely mixed:
- Phase split in tags-filter (PASS1 → CLOSURE → WAYDEPS) means way
  blobs are read twice. ~8-10 seconds at Europe scale.
- Possibly columnar scratch allocation in PASS1 even where unused.
- Lost producer/consumer overlap from accumulate's all-finish-then-merge
  barrier in extract-smart PASS2.
- Cross-day measurement noise on `--bench 1` runs from different
  commits.

**Investigation deferred** until after the extract.rs:2813 fix lands.
The fastest isolation path is `--bench 3` baselines on the same day
followed by per-call-site A/B toggles on HEAD. Do not start with
flamegraphs.

### Future optimization (not needed now)

Contiguous worker sharding and batch set_sorted_batch remain viable
for future optimization if consumer throughput becomes a bottleneck.
Both reviewers agree: do not build these yet. Address only if
profiling shows consumer merge is the dominant cost.
