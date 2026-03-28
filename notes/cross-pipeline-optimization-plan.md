# Cross-pipeline optimization plan

Patterns discovered during the external join OOM investigation that apply
across the pbfhogg codebase. Updated with sweep review findings (8 reviewers,
commit `65bc639`).

## Completed work

**Group 1: Infrastructure** — ALL DONE
- Document PrimitiveBlock retention footgun in pipeline.rs (`a067759`)
- Shared `node_scanner.rs` with `extract_node_tuples` / `NodeTuple` (`b3e8bf7`)
- Sidecar profiler with `emit_marker` / `emit_counter` (replaces debug-logging)
- BlobReader fadvise gated on `target_os = "linux"` (`7acbb1a`)

**Group 2: Planet blockers** — ALL DONE
- build-geocode-index: sequential reader for pass 2 (`5776b67`). Anon=325 MB.
- ALTW dense/sparse pass 1: node-only scanner (`b3e8bf7`). Japan 69s→42s.
- diff + derive-changes: sequential readers (`6d996f6`).
- check-refs: sequential reader (`fb8dd3c`). Anon 2.9 GB→581 MB.

**Group 3: External join P2b/P2c** — ALL DONE
- P2b-v2: pread-from-workers for stage 2 (`80e227b`). Stage 2: 301s→216s, anon 20.4→1.4 GB.
- P2c: parallel assembly for stage 4 (`6b09796`). Stage 4: 432s→136s, 7.3 GB anon.
- Sequential reader for stage 1 (`4daf995`). Anon 11 GB→70 MB.
- Planet validated (`98e71e2b`): 1,462s (24.4 min), 16.7 GB peak anon. 3.9x faster than dense.

**Group 5: Polish** — DONE
- Sparse deprecation hint on sorted indexed PBFs (`cb57493`)
- `--index-type auto`: external if sorted+indexed, dense otherwise (`cb57493`)
- `debug-logging` feature removed, replaced with `emit_counter` (`65bc639`)

**Geocode builder** — DONE
- Pass 1.5: referenced-node collection via IdSetDense (`c5c44b1`)
- Compact rank-indexed coord array replacing DenseMmapIndex (`7cf2239`).
  Europe: 3,411s→568s (6x), RSS 24.5→7.5 GB.

## Remaining work

### ~~Priority 1: Unbatched pipelined consumers~~ — DONE

| Command | Before | After | Commit |
|---------|--------|-------|--------|
| tags-filter two-pass (pass 1 + closure + way deps) | OOM (24.3 GB anon) | **2.2 GB** | `c6c13ff` |
| getid --add-referenced (pass 1) | 8.3 GB anon | **357 MB** | `142d7eb` |

Converted all unbatched collection passes to sequential BlobReader +
DecompressPool. Rewrite passes still use batched `par_iter` (safe).

### Priority 2: Scanner family

Way-ref-only and relation-member scanners would avoid full PrimitiveBlock
construction in several passes that only need refs/members:

| Target | Current | Scanner needed |
|--------|---------|---------------|
| ALTW pass 0 (`collect_way_referenced_node_ids`) | Pipelined PrimitiveBlock | Way-ref scanner |
| Geocode pass 1.5 (referenced node collection) | Sequential PrimitiveBlock | Way-ref scanner |
| `collect_relation_member_node_ids` | Pipelined PrimitiveBlock | Relation-member scanner |
| merge `--locations-on-ways` node mining | PrimitiveBlock | Node-coordinate scanner |

These are lower priority — the pipelined passes process filtered blobs
(30% for ways, 5% for relations) and the retention is bounded by pipeline
depth. But a way-ref scanner would generalize nicely.

### Priority 3: Monitor at planet scale

Commands that haven't been sidecar-validated at planet. Run with
`--bench` and check anon trajectory before trusting at planet scale.

| Command | Expected risk | Notes |
|---------|--------------|-------|
| merge (apply-changes) | Medium | Production path. Budgeted batching should bound retention. |
| tags_count | Low | Pipelined, analytics-only |
| verify_ids | Low | Pipelined, validation-only |
| renumber | Low | Pipelined, rarely used at scale |

### Not planned (sweep review consensus)

**Batched consumers (extract, tags-filter rewrite, cat --type, getid rewrite):**
Sidecar showed flat anon at Europe. Batch window (64 blocks) bounds in-flight
retention regardless of total blob count. 8/8 reviewers agree: don't convert
preemptively. Monitor at planet if needed.

**ALTW dense → compact rank-indexed array:** DenseMmapIndex is correct for
dense — ~90% of nodes are referenced, so there's no sparsity to exploit.
The rank() overhead would slow the hot path for no memory savings. Users on
memory-constrained hosts use `--index-type external`. Perf-Codex disagrees
(thinks it's worth benchmarking) but other 7 reviewers say no.

**Pread-from-workers for remaining commands:** The pattern solves decode
parallelism + retention, but the remaining commands are either consumer-bound
(check-refs), already have parallel assembly via rayon batches (extract,
tags-filter), or are too niche (node_stats). Sequential reader is the right
fix where retention is the concern; pread-from-workers is overkill.

**Sort:** Already uses custom sequential frame scanning, not pipelined decode.

## Patterns reference

### Pread-from-workers
IO thread reads headers only, workers pread blob data from shared `Arc<File>`,
all alloc/free thread-local. Eliminates cross-thread retention AND parallelizes
decode. Used in external join stages 2+4. Best for decode-bound pipelines.

### Compact rank-indexed coord array
IdSetDense with two-level prefix-sum `rank()` replaces sparse DenseMmapIndex.
Turns scattered mmap (128 GB virtual, 20+ GB RSS) into contiguous (16 GB).
Used in geocode builder. Best when referenced nodes are a small fraction of
total (~1-10%).

### Sequential BlobReader + DecompressPool
All alloc/free on one thread. No cross-thread retention. Used for consumer-bound
commands (check-refs, diff, derive-changes, geocode pass 2, external join stage 1).

### Node-only wire scanner
Bypasses PrimitiveBlock for node id/lat/lon. Zero per-block heap allocations.
Used in external join stage 2, ALTW dense/sparse pass 1.

### Ref count sidecar
Stage 1 writes per-blob metadata to scratch file, later stages read it for
pre-computation. Used in external join stage 4 for slot_pos. Generalizable to
any multi-stage pipeline needing per-blob data from an earlier pass.

### emit_marker + emit_counter
All observability through sidecar FIFO. Phase markers for timing, counters for
application-level data. Binary is silent except errors. Replaces debug-logging.
