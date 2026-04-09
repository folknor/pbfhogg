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

## Next steps

The remaining alloc opportunity for multi-extract is the per-worker
Vec growth (8.7 GB at Japan scale). Options:
1. **Contiguous worker sharding** — assign each worker a contiguous
   schedule slice instead of shared work queue. Per-worker IdSetDense
   chunks would be non-overlapping, making merge zero-copy. Eliminates
   Vec intermediary entirely without the random-access penalty (each
   worker's IDs fall in a narrow chunk range).
2. **Batch set_sorted_batch()** — amortize chunk lookup for runs of
   IDs in the same chunk. Less effective than #1 but simpler.
3. **Accept the Vec pattern** — 8.7 GB at Japan is bounded and
   proportional to input size. The per-worker accumulation already
   eliminated per-blob channel alloc.
