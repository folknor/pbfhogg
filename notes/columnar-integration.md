# Columnar decode integration research

## Current state

Prototype shipped (commit `e0b0780`). `DenseNodeColumns` in
`src/read/columnar.rs` batch-decodes IDs, lats, lons into contiguous
arrays. Two classification methods:
- `collect_matching_ids_bbox` - single-region bbox, output to Vec
- `collect_matching_ids_multi_bbox` - N-region bbox, output to Vec

Used in:
- Single-extract node classification for bbox regions (Vec path)
- Multi-extract node classification for all-bbox regions (Vec path)

The IdSetDense output methods (`set_matching_ids_bbox`,
`set_matching_ids_multi_bbox`) were removed in commit `c4c7b9e` after
confirming they were unused in production. Direct IdSetDense::set() in
tight loops caused 29x regression vs Vec::push() due to random chunk
access. See "IdSetDense accumulation findings" below.

ASM inspection confirms LLVM does NOT autovectorize the bbox loop -
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
in `parallel_classify_phase` - workers accumulate Vec<i64> across all
blobs and send once at completion. No per-blob allocation through the
channel.

### parallel_classify_phase refactor

Workers now own persistent state `S` via `worker_init`. The classify
closure mutates `S` without returning per-blob results. Merge receives
the final `S` once per worker at scope exit.

For hot paths (node/way classify): S = Vec or Vec<Vec> - sequential
push, cache-friendly, consumer iterates into IdSetDense.

For sparse paths (relation classify, way dep scans, geocode): S =
IdSetDense - direct set() is fine since filter work dominates and
match counts are low. Merge uses `IdSetDense::merge` (bitwise OR,
zero-copy for non-overlapping chunks).

## IdSetDense accumulation findings (commit `e94c3c8`)

Direct `IdSetDense::set()` in tight classify loops:
- **Alloc: 20.5 MB** (down from 8.7 GB) - 99.8% reduction
- **Node classify: 20.5s** (up from 713ms) - 29x regression
- **Way classify: 3.7s** (up from 943ms) - 4x regression

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

### 1. Multi-extract node classification - DONE

`collect_matching_ids_multi_bbox` tests each node against all N
bboxes in one pass over contiguous i32 lat/lon arrays. Per-worker
`DenseNodeColumns` + `Vec<Vec<i64>>` reused across blobs. Polygon
regions fall back to element-by-element.

### 2-5. ALTW, external join, geocode, node stats - NOT GOOD TARGETS

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
- `parallel_classify_phase<S, R>` - per-blob sends with scratch
- `parallel_classify_accumulate<S>` - per-worker accumulation (current)

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

### Superseded (2026-04-11 investigation, closed)

The "Resolved" framing below (the per-call-site way-dep table, the
chunk-spread model, the planet-blocker prediction) was the 2026-04-09
design review's working theory. It was investigated over four rounds in
2026-04-10/11. **The diagnosis was wrong end-to-end.** The actual
mechanism (cold-arena-page residency cascade triggered by post-PASS1
phases touching glibc's bloated free-list) is unrelated to the
chunk-spread model and unrelated to `extract.rs:2813` specifically.

The architectural fix (`extract.rs:2813` → per-blob send, commit
`cc19d26`) was kept because it's still correct in principle and improved
PASS2 wall by ~23%. The schedule-reuse pattern shipped in commits
`d4ea760` (PASS2) and `0b085b1` (PASS3) eliminated the post-PASS1 header
scans that triggered the worst residency cascades, producing a cumulative
~29% wall improvement on Europe smart extract. The planet measurement
(commit `cadc3e6`, UUID `2d028196`, Europe bbox) came in at 11.17 GB
peak anon / 279 s wall - comfortable on 27 GB hosts, so the investigation
closed with "ship as-is" and no mitigation-menu item was needed.

### Future optimization (not needed now)

Contiguous worker sharding and batch set_sorted_batch remain viable
for future optimization if consumer throughput becomes a bottleneck.
Both reviewers agree: do not build these yet. Address only if
profiling shows consumer merge is the dominant cost.
