# Columnar decode integration research

## Current state

Prototype shipped (commit `e0b0780`). `DenseNodeColumns` in
`src/read/columnar.rs` batch-decodes IDs, lats, lons into contiguous
arrays. Four classification methods:
- `collect_matching_ids_bbox` — single-region bbox, output to Vec
- `collect_matching_ids_multi_bbox` — N-region bbox, output to Vec
- `set_matching_ids_bbox` — single-region bbox, output to IdSetDense
- `set_matching_ids_multi_bbox` — N-region bbox, output to IdSetDense

Used in:
- Single-extract node classification for bbox regions (Vec path)
- Multi-extract node classification for all-bbox regions (Vec path)

The IdSetDense output methods exist but are NOT used in production —
direct IdSetDense::set() in tight loops causes 29x regression vs
Vec::push() due to random chunk access. See "IdSetDense accumulation
findings" below.

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
| Way dep node refs (tags_filter) | IdSetDense | 1.5 GB | Disputed | See below |
| Way dep node refs (smart extract) | IdSetDense | 1.5 GB | Disputed | See below |
| Geocode referenced nodes | IdSetDense | 1.5 GB | Disputed | See below |

### Disputed: way dep IdSetDense accumulation

Split opinion on per-worker IdSetDense for way dep node refs:
- **Keep (correctness/claude):** 6 workers × 1.5 GB = 9 GB, feasible
  on 30 GB host. Merge frees 5 copies → 1.5 GB final.
- **Not safe (perf/codex, arch/codex, planet):** IdSetDense memory
  is driven by chunk spread, not count. Shared work queue means each
  worker touches the full node ID range → each allocates ~387 chunks
  = ~1.5 GB. 6 × 1.5 GB = 9 GB is tight alongside other allocations.

Decision pending. If we keep it, need planet-scale measurement to
confirm 9 GB fits. If not, revert to per-blob send (each blob
produces ~64K refs, bounded).

### Future optimization (not needed now)

Contiguous worker sharding and batch set_sorted_batch remain viable
for future optimization if consumer throughput becomes a bottleneck.
Both reviewers agree: do not build these yet. Address only if
profiling shows consumer merge is the dominant cost.
