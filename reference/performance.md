# Performance Data

Consolidated runtime measurements across datasets and commands.

## Host: plantasjen

- CPU: AMD (details via `brokkr env`)
- RAM: 30 GB
- Swap: 8 GB
- Storage: nvme (source, data, scratch), hdd (target/cargo)
- Governor: performance
- Profile: `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`

## Datasets

| Dataset | Raw PBF | Indexed PBF | ALTW PBF | Elements |
|---------|---------|-------------|----------|----------|
| Malta | 8 MB | 8 MB | — | ~1M |
| Greater London | 122 MB | 122 MB | — | ~17M |
| Denmark | 461 MB | 465 MB | — | 59M |
| Switzerland | 524 MB | — | — | — |
| Norway | 1.4 GB | 1.4 GB | — | — |
| Japan | 2.4 GB | 2.4 GB | — | 344M |
| Germany | 4.5 GB | 4.5 GB | — | — |
| North America | 18.8 GB | 18.8 GB | — | 2.58B |
| Europe | 32.4 GB | 33.6 GB | — | 4.2B (3.7B nodes, 454M ways, 8.2M rels) |
| Planet | 87.3 GB | 87.7 GB | 88.4 GB | 11.6B (10.4B nodes, 1.17B ways, 14.1M rels) |

## Cat passthrough (indexdata generation)

No `--type` filter. Decompresses each blob to scan IDs/tags, reframes BlobHeader
with indexdata+tagdata, preserves original compressed bytes. No re-compression.

Commit `69a127f`, plantasjen.

| Dataset | Size | Buffered | `--direct-io` | File size overhead |
|---------|------|----------|---------------|--------------------|
| Denmark | 461 MB | **2.8s** | — | — |
| Europe | 32.4 GB | — | 112s* | +3.8% |
| Planet | 87 GB | **497s** (8m17s) | 520s (+5%) | +0.5% |

\* Europe used `--type node,way,relation` (filtered path, full decode+re-encode),
not passthrough. Passthrough not yet measured for europe.

Buffered wins for passthrough — sequential single-file I/O benefits from page
cache prefetch. `--direct-io` adds alignment overhead without the concurrent
read/write pattern that makes it faster for merge.

The `--type` filtered path (full decode+re-encode) **OOMs on planet** (87 GB) on
30 GB host at ~25% through. Pipelined writer's rayon pool lacks backpressure.
Works on europe (32.4 GB).

## Read throughput

Count all elements, best of 3 runs. Commit `d387301` (multi-dataset), plantasjen.

| Dataset | Size | sequential | parallel | pipelined | blobreader | mmap |
|---------|------|-----------|----------|-----------|------------|------|
| Malta | 9 MB | 49 ms | 9 ms | 24 ms | 50 ms | 52 ms |
| Denmark | 487 MB | 2.86s | 463 ms | 1.46s | 2.93s | 2.93s |
| Norway | 1.4 GB | 8.4s | 1.33s | 4.9s | 8.9s | 8.8s |
| Japan | 2.4 GB | 14.5s | 2.1s | 8.0s | 15.2s | 15.2s |
| Germany | 4.7 GB | 26.9s | 4.2s | 13.0s | 27.8s | 27.6s |

North America (18.8 GB, 2.58B elements, commit `a6ebbfe`):
parallel 22s, pipelined 57s, sequential 130s.

## Write throughput

Decode all elements then write through BlockBuilder + PbfWriter to `/dev/null`.
Commit `d387301` (multi-dataset), plantasjen.

| Dataset | Size | sync-none | sync-zlib:6 | sync-zstd:3 | pipelined-none | pipelined-zlib:6 | pipelined-zstd:3 |
|---------|------|-----------|-------------|-------------|----------------|------------------|------------------|
| Malta | 9 MB | 136 ms | 282 ms | 172 ms | 123 ms | 130 ms | 128 ms |
| Denmark | 487 MB | 8.1s | 16.8s | 10.0s | 7.3s | 7.4s | 7.3s |
| Norway | 1.4 GB | 21.3s | 44.0s | 25.7s | 18.9s | 19.2s | 18.9s |
| Japan | 2.4 GB | 38.5s | 79.2s | 47.0s | 34.8s | 35.0s | 34.4s |
| Germany | 4.7 GB | 81.3s | — | — | 71.7s | — | — |

With pipelined writes, all compression modes converge to the decode + wire-format
serialization floor. Sync zlib:6 is 2.3x slower; pipelined hides the cost.

North America (18.8 GB, 2.58B elements, commit `a6ebbfe`):
pipelined zlib 4m27s, pipelined none/zstd ~4m20s, sync zlib 14m34s.

## Merge (apply-changes)

Best results per dataset. Commit `a6ebbfe` (NA), `a65a198` (multi-region),
`e7bbfa2` (Denmark latest). Plantasjen.

| Dataset | Size | buffered+none | buffered+zlib | uring+none | uring+zlib |
|---------|------|---------------|---------------|------------|------------|
| Malta | 9 MB | 14 ms | 42 ms | — | — |
| Greater London | 124 MB | 140 ms | 333 ms | — | — |
| Denmark | 487 MB | 218 ms | 331 ms | — | — |
| Switzerland | 529 MB | 561 ms | 1.22s | — | — |
| Norway | 1.4 GB | 549 ms | 747 ms | — | — |
| Japan | 2.4 GB | 1.87s | 2.88s | — | — |
| Germany | 4.7 GB | 3.42s | 5.34s | 4.4s | 9.6s |
| North America | 18.8 GB | 14.9s | 17.3s | **11.9s** | 15.2s |
| Planet | 87 GB | 515s | 762s | — | — |

Germany (4.7 GB, 146K-change daily diff): rewrite fraction 18.4%.
North America (18.8 GB, 645K-change daily diff): 303K passthrough / 19.6K
rewritten blobs. All variants under 600 MB RSS.
Planet (87 GB, daily diff): 86% rewrite, 1.8 GB RSS.

io_uring crossover at ~4-5 GB input. Below that, page cache absorbs everything.
At NA scale (18.8 GB exceeds 30 GB page cache), O_DIRECT + async I/O delivers
12-20% improvement. sqpoll adds no measurable benefit (<1%).

### Cross-pipeline optimization: skip_metadata in block_overlaps_diff

Commit `b90e8ef`: use `elements_skip_metadata()` in `block_overlaps_diff`
(only accesses element IDs, not metadata). Single-line change.

Germany hotpath (commit `1b10f18`, plantasjen):
- apply-changes-zlib: **6942ms → 5928ms (-15%)**

Larger improvement than expected — Germany's 18.4% rewrite fraction means
more blobs reach the precise `block_overlaps_diff` check (which decodes all
elements to test IDs against the diff). Skipping metadata decode saves ~1s
across ~11K precise-check invocations.

## Add-locations-to-ways

Dense mmap index: 16B slots × 8 bytes = 128 GB virtual address space.
Only touched slots consume physical memory.

Commit `69a127f`, plantasjen (30 GB RAM, 8 GB swap).

### Europe (33.6 GB indexed, 4.2B elements)

3.7B nodes read, 149M written, 3.57B dropped. 453M ways, 8.2M relations.
1029 passthrough blobs, 521K decoded. 0 missing locations.

| I/O Mode | Time |
|----------|------|
| Buffered | **2565s** (42m45s) |
| `--direct-io` | 2611s (+2%) |

### Planet (87.7 GB indexed, 11.6B elements)

10.4B nodes read, 285M written, 10.2B dropped. 1.17B ways, 14.1M relations.
452 passthrough blobs, 50K decoded. 0 missing locations.
Output: 88.4 GB (+0.7% from embedded way-node coordinates).

| I/O Mode | Time |
|----------|------|
| Buffered | **5773s** (96m) |

Planet on 30 GB host with 8 GB swap — memory-latency-bound (page faults on
sparse mmap index), not compute-bound. Production host (64 GB RAM) should be
well under an hour.

`--direct-io` provides no benefit for ALTW — workload is compute/memory-bound,
not I/O-bound. Sequential I/O benefits from page cache prefetch.

### Dense vs Sparse vs External index (plantasjen)

| Dataset | Dense | Sparse | External | Commit |
|---------|-------|--------|----------|--------|
| Denmark (465 MB) | **6.8s** | 14.1s | 14s | `ee9b19f` |
| Japan (2.4 GB) | **42s** | — | — | `b3e8bf7` (node scanner) |
| Europe (33.6 GB) | 2,940s (49m)* | 6,453s (107m) | **577s (9.6m)** | `6b09796` |
| Planet (87.7 GB) | 5,773s (96m)* | — | **1,462s (24.4 min)**, 16.7 GB peak anon | `98e71e2b` (sidecar) |

*Dense at Europe scale thrashes on 30 GB host (mmap working set ~16 GB > available
RAM). Japan 42s is with node-only scanner for pass 1 (commit `b3e8bf7`, previously
72s with pipelined PrimitiveBlock). Europe 2,940s is also with node scanner but
mmap thrashing dominates.

*Planet with dense thrashes on 30 GB host (memory-latency-bound).

Dense is fastest when the working set fits in RAM. External uses ~1.6 GB
anon RSS at Europe scale via 4-stage radix join pipeline (node-only wire
scanner for stage 2, scatter buffer for stage 3, sequential reader for
stage 4).

**Crossover point**: between Japan (2.4 GB, dense 2x faster) and Europe
(33.6 GB, external 2.8x faster). At Europe scale, dense's mmap working set
(~16 GB) exceeds available RAM, causing thrashing. External's sequential
I/O stays bounded.

### External join stage breakdown (Europe, commit `6b09796`, plantasjen)

| Stage | Time | RSS (anon) | Description |
|-------|------|-----------|-------------|
| Stage 1 (way pass) | 81s | 69 MB post | Pipelined reader + BufWriter buckets |
| Stage 2 (node join) | 216s | 69 MB post | Node-only sequential scanner + merge-join |
| Stage 3 (slot reorder) | 78s | 69 MB post | Scatter buffer (was 1079s with pwrite) |
| Stage 4 (assembly) | 136s | 1.6 GB flat | Sequential reader + batch parallel assembly |
| **Total** | **577s** | | With `--compression none`: ~754s |

### External join optimization history

| Version | Denmark | Europe | Commit |
|---------|---------|--------|--------|
| Original (256x re-read) | 302s | — | `034422c` |
| Single-pass merge | 25s | 2,060s | `a334c72` |
| + fadvise + mmap coord_slots | 22s | 1,824s | `165cbb2` |
| Node-only scanner + scatter buffer | 14s | 921s | `ee9b19f` |
| + blob skip + pool reuse | 14s | ~901s | `d272b49` |

Key optimizations: node-only wire scanner (bypasses PrimitiveBlock, eliminates
25 GB heap retention), scatter buffer (eliminates sort + 4.69B pwrite calls,
15x speedup), BlobReader fadvise(DONTNEED) (general infrastructure), deferred
IdSetDense, buffer reuse in bucket loads.

See [altw-optimization-history.md](altw-optimization-history.md) for the
full investigation and memory optimization research log.

## CLI commands

Commit `aacbe80`, plantasjen. Best of 3 runs.

### Denmark (487 MB indexed, 59M elements, commit `6fc1283`, plantasjen)

| Command | Time |
|---------|------|
| tags-filter-osc | 14 ms |
| inspect (indexdata) | 19 ms |
| cat --type relation | 85 ms |
| tags-filter highway=primary | 152 ms |
| inspect-tags --type way | 251 ms |
| sort (sorted, indexdata) | 366 ms |
| getid | 379 ms |
| getparents | 400 ms |
| tags-filter amenity=* | 438 ms |
| apply-changes | 517 ms |
| cat --type way | 239 ms |
| merge-changes | 107 ms |
| inspect-tags | 1.61s |
| inspect-nodes | 1.73s |
| check --ids | 1.87s |
| getid --invert | 0.5s |
| extract --simple | 2.48s |
| extract --complete | 2.40s |
| tags-filter two-pass | 2.62s |
| extract --smart | 2.65s |
| add-locations-to-ways | 5.59s |
| check --refs | 6.83s |
| time-filter | 9.39s |
| cat --dedupe | 22.4s |
| renumber | 22.3s |

### Japan (2.4 GB indexed, 344M elements, plantasjen)

Baseline commit `aacbe80`. Entries marked with † were improved by later
optimizations and show the latest measured value.

| Command | Time | Notes |
|---------|------|-------|
| inspect (indexdata) | 92 ms | |
| tags-filter-osc | 169 ms | |
| cat --type relation | 306 ms | |
| cat --type way | 0.7s | † raw passthrough, `c33e8cc` |
| tags-filter highway=primary | 840 ms | |
| sort (sorted, indexdata) | 1.33s | |
| getid --invert | 1.3s | † raw passthrough, `c33e8cc` |
| merge-changes | 1.62s | |
| getid | 1.94s | |
| getparents | 2.06s | |
| tags-filter amenity=* | 2.20s | |
| inspect-tags --type way | 2.43s | |
| apply-changes | 2.53s | |
| extract --complete | 4.4s | † parallel classify |
| inspect-tags | 4.82s | |
| extract --smart | 5.2s | † parallel classify |
| inspect-nodes | 9.14s | |
| extract --simple | 9.36s | |
| check --ids | 10.4s | |
| tags-filter two-pass | 13.7s | |
| check --refs | 38.7s | |
| time-filter | 43.8s | |
| add-locations-to-ways | 64.1s | |
| diff | 72.2s | |
| diff --format osc | 73.1s | |
| cat --dedupe | 102.2s | |
| renumber | 152.4s | |

### Germany (4.7 GB indexed, ~496M elements)

Hotpath profiling, commit `1b10bfd`, plantasjen.

| Test | Time | RSS | Notes |
|------|------|-----|-------|
| inspect-tags | 23.9s | 1.6 GB | decompress_blob 28.7s cumulative (parallel), pipeline 12.1s |
| check-refs | 74.1s | 4.6 GB | 99.97% in pipeline, single-threaded consumer bound |
| cat --type (zlib) | 61.8s | 10.9 GB | frame_blob 193s cumulative (parallel zlib), add_node 22.6s (429M), add_way 22.8s (70M) |
| apply-changes zlib | 6.2s | 395 MB | classify 2.9s, rewrite+output 2.1s |
| apply-changes none | 4.4s | 252 MB | classify 1.2s, rewrite+output 1.9s |

Allocation profiling (same commit):

| Test | Net Alloc | Cumulative | Key finding |
|------|-----------|------------|-------------|
| inspect-tags | 3.0 GB | 25.7 GB | decompress_blob 5.1 GB, wire::parse 3.1 GB |
| check-refs | 2.4 GB | 4.0 GB | wire::parse 3.0 GB (126%), nearly all in block::new |
| cat --type (zlib) | 175 MB | 240 GB | take_owned 41 GB, add_way 14.8 GB, decompress 6.9 GB |
| merge zlib | 293 MB | 29.6 GB | rewrite_block_parallel 17.3 GB, read_raw_frame 4.4 GB |
| merge none | 293 MB | 31.7 GB | same pattern, RSS under 300 MB |

Previous commit data (commit `46f7388`):

| Command | Time |
|---------|------|
| add-locations-to-ways | 64.5s |

### vs osmium (Denmark, commit `23862d1`)

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| sort (sorted, indexdata) | **0.14s** | 11.6s | **83x** |
| apply-changes (indexdata + zlib) | **2.7s** | 7.2s | **2.7x** |
| tags-filter w/highway=primary -R | **0.24s** | 0.56s | **2.3x** |
| cat --type way (indexdata, raw passthrough) | **0.24s** | 2.22s | **9.3x** |
| add-locations-to-ways | **8.3s** | 12.6s | **1.5x** |
| check --refs | **4.8s** | 4.5s | 0.94x |

## Renumber (external mode)

Planet-scale renumber via IdSetDense rank-based O(1) lookup (replaces
the original 256-bucket radix partition). Wire-format splice rewriters
for all three element types — pass 1 (DenseNodes), stage 2d (ways),
and R2d (relations) — patch only the ID/ref fields and copy everything
else verbatim as raw bytes. No BlockBuilder, no PrimitiveBlock
construction. Pass 1: 4 work-stealing workers. Stage 2d: 6 workers.
R2d: parallel with inline rank() dispatch (relation_map replaced by
`relation_id_set.rank()`). All member-ref lookups via
`node_id_set.rank()` + `way_id_set.rank()` inline — no flat temp
files. Zero scratch disk usage. Single shared input fd across all
phases. Atomic index dispatch (no `Arc<Mutex<Receiver>>`). Output
defaults to zlib:1. `mallopt(M_ARENA_MAX, 2)` inside
`renumber_external()` prevents glibc cross-thread arena fragmentation.

### Planet (87.7 GB indexed, 11.6B elements, plantasjen)

Commit `6165394`, dirty `--force --bench 1`. Single-sample.

| Phase | Duration | Peak Anon | Share |
|---|---:|---:|---:|
| PASS1 nodes | **169 s** | 610 MB | 15.5% |
| STAGE2A way emit | **119 s** | 934 MB | 10.9% |
| STAGE2B node merge-join | **381 s** | **7.32 GB** | 34.9% |
| STAGE2C slot reorder | **146 s** | 3.03 GB | 13.4% |
| STAGE2D way assembly | **129 s** | 809 MB | 11.8% |
| R1+R2A fused | 29 s | 1.04 GB | 2.7% |
| R2B rel merge-join | 68 s | 2.03 GB | 6.2% |
| R2C + R2D | 34 s | 1.04 GB | 3.1% |
| **TOTAL** | **1,092 s (18.2 min)** | **7.32 GB** | — |

Stage 2b breakdown (cumulative across 2 workers):
  load_way_refs 276 s, radix_sort 243 s, load_node_map 101 s,
  merge_join 130 s. Stage 2b is the #1 remaining target.

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways / 14,124,889
relations / 12,435,459,911 way refs. All match the first-measurement
baseline (`c5d00c22`) exactly.

### Optimization history

| Commit | Change | Planet Time |
|--------|--------|-------------|
| `e156e97` | First planet measurement (sequential all stages) | **3,456 s (57.6 min)** |
| `cc80442` | Stage 2b LSD radix sort | — (Denmark only) |
| `a478ae8` | Halve map-bucket format (drop new_id field) | — |
| `37ff902` | Stage 2b 2-worker bucket parallelism | — |
| `8ec298c` | Pass 1 parallel decode (worker pool) | — |
| `34a6b7c` | Stage 2d parallel decode (worker pool) | — |
| `e7219f0` | Stage 2a parallel scan (worker pool) | — (OOM on planet, see below) |
| `9695ad5` | Writer backpressure (permit pool) | — (still OOM) |
| `f607842` | Work-stealing dispatch for pass 1 + stage 2d | **2,033 s (33.9 min)** |
| `d3da65f` | Two-cursor merge + PrimitiveBlock copy fix | **1,901 s (31.7 min)** |
| `dc13a7b` | DenseNodes wire-format rewriter + 4 workers + mallopt | **1,468 s (24.5 min)** |
| `48183b5` | Way wire-format rewriter for stage 2d | **1,334 s (22.2 min)** |
| `dc13a7b` | DenseNodes rewriter + 4 workers + mallopt | **1,468 s (24.5 min)** |
| `d11166b` | Stage 2d 4 workers | **1,325 s (22.1 min)** |
| `6165394` | 14-opt batch: splice, parallel 2c, schedule reuse, batch writes | **1,092 s (18.2 min)** |
| `7839303` | Stage 2b/2c 4 workers + radix 4 passes | **960 s (16.0 min)** |
| `9ec5eda` | IdSetDense rank fusion (eliminates stage 2a+2b+2c) | **505 s (8.4 min)** |
| `c5c0e08` | Build way_id_set during stage 2d | **479 s (8.0 min)** |
| `ae45fd6` | Eliminate way_map files + mmap R2B scatter | **442 s (7.4 min)** |
| `94bf351` | Pass 1 back to 4 workers, fuse R1+R2A+R2B | **442 s (7.4 min)** |
| `cbffb45` | Wire-format splice rewriter for R2d relations | **412 s (6.9 min)** |
| `71bb548` | Parallel R2d (work-stealing + member-count sidecar) | **401 s (6.7 min)** |
| `dd3f477` | zlib:1 output + IdSetDense::resolve() combined lookup | — |
| `1b171f0` | Inline IdSetDense::set() during reframe, eliminate old_ids_out | — |
| `fefd357` | Cache blob schedules across all phases | **360 s (6.0 min)** |
| `b71bae9` | Fuse relation resolve into R2d, eliminate all temp files, zero scratch disk | — |
| `feb3099` | Denser rank() blocks (64B instead of 256B) + respect compression flag | — |
| `6acb9eb` | Replace relation_map FxHashMap with IdSetDense (~500 MB → ~20 MB) | — |
| `db49c92` | Open input file once, reuse fd across all phases | — |
| `67c7960` | Atomic index dispatch + reframe_buf pre-reserve | **209 s (3m29s)** |

**−3,247 s (−94%)** from baseline. Each commit verified on Denmark
(`brokkr verify renumber`, 306-relation orphan delta preserved exactly).
Two intermediate planet runs OOM-killed at ~26 GB anon RSS due to
reorder-buffer backlog from range-split dispatch and glibc arena
fragmentation — resolved by work-stealing dispatch + `MALLOC_ARENA_MAX=2`.

### Memory

Peak anon 7.0 GB (commit `67c7960`). Dominated by IdSetDense bitsets
(node_id_set ~1.6 GB + rank index ~1 GB, way_id_set ~200 MB + rank,
relation_id_set ~20 MB). No FxHashMap relation_map — replaced by
IdSetDense. Zero temp disk (all flat files eliminated). `mallopt
(M_ARENA_MAX, 2)` inside `renumber_external()` caps glibc arena growth
from cross-thread OwnedBlock `Vec<u8>` frees. Well under the 30 GB
host limit.

### Phase breakdown (commit `67c7960`, planet, `--bench 1`)

| Phase | Duration | Peak Anon | Share |
|---|---:|---:|---:|
| PASS1 (4 workers, wire-format nodes) | **124 s** | — | 59% |
| STAGE2D (6 workers, fused way resolve + wire-format ways) | **77 s** | — | 37% |
| R1+R2A (sequential relation ID assignment) | **4.4 s** | — | 2% |
| R2D (parallel wire-format relations, inline rank()) | **2.0 s** | — | 1% |
| **TOTAL** | **209 s (3m29s)** | **7.0 GB** | — |

## Extract

Plantasjen. Best of 3 runs (or single-sample where noted), indexed PBFs.

| Dataset | Size | simple | complete | smart | Commit |
|---------|------|--------|----------|-------|--------|
| Denmark | 487 MB | 2259 ms | 2399 ms | 2693 ms | `aacbe80` |
| Japan | 2.4 GB | **3.8s** | **3.7s** | **4.7s** | `cadc3e6` |
| Europe | 32.4 GB | **96.3s** | **164.9s** | **181.4s** | `cadc3e6` |
| Planet † | 87.7 GB | — | — | **279s** | `cadc3e6` |

† Planet smart extract: single-sample `--bench 1`, Europe bbox, UUID
`2d028196`. Peak anon RSS 11.17 GB on 32 GB host (27.9 GB avail at run
start, 16.7 GB headroom to the round-4 "ship as-is" threshold of
~25 GB). See [notes/parallel-classify-regression.md](../notes/parallel-classify-regression.md)
for the full planet measurement write-up and mechanism analysis.

Denmark bbox `12.4,55.6,12.7,55.8`, Japan bbox `139.5,35.5,140.0,36.0`,
Europe and Planet bbox `-25.0,34.0,45.0,72.0` (full-continent).

Simple extract uses a 3-phase barrier pipeline with parallel classification
and raw frame passthrough. Each phase (nodes, ways, relations) classifies
blobs in parallel then writes matching raw frames via pread workers — no
decode+re-encode. Japan simple: 3.8s vs osmium 7.2s (1.9x faster). Europe
simple: 96.3s (was 350s sequential, was OOM with pipelined reader).

Complete-ways and smart pass 1 (`collect_pass1_generic`) uses three-phase
parallel pread classification (nodes → ways → relations) via a reusable
`parallel_classify_phase` helper. Smart pass 2 (way dependency resolution)
also uses `parallel_classify_phase`, replacing the old sequential BlobReader
scan. Workers pread + decompress + classify in parallel, sending compact
results back to the consumer. Japan complete: 19.7s → 3.7s (5.3x), smart:
24.3s → 4.7s (5.2x). Both beat osmium (complete 2.5x faster, smart 2.6x
faster at earlier measurements). Write passes use pread-from-workers with
full PrimitiveBlock lifecycle per worker.

**PASS1 schedule reuse (commits `d4ea760`, `0b085b1`, 2026-04-10/11).** The
parallel_classify_regression investigation discovered that every header
scan running *after* PASS1's parallel allocator work was redundant —
`collect_pass1_generic` already scans the whole file once. By plumbing
`full_way_schedule` and `pass3_blob_schedule` out of `collect_pass1_generic`
via `Pass1Result` and consuming them via `mem::take` in PASS2/PASS3, smart
extract now does ONE file scan instead of THREE. Europe impact at
commit `cadc3e6` vs pre-investigation `fc17b51`:

| Strategy | Pre-investigation | Post | Δ |
|---|---|---|---|
| smart | 208.2s (`fc17b51`) | **181.4s** | **−13%** (−29% vs mid-investigation `5ca2df9` peak of 254s) |
| complete | 198.0s (`fc17b51`) | **164.9s** | **−17%** |
| simple | 113.1s (`fc17b51`) | **96.3s** | **−15%** |

Complete benefits because `extract_complete_ways` PASS2 now also consumes
`pass3_blob_schedule` via `pread_write_pass_with_schedule`. Simple benefits
from shared instrumentation and scan-path improvements in the same commit
range. See [notes/parallel-classify-regression.md](../notes/parallel-classify-regression.md)
for the full investigation, including the cold-arena-page residency
cascade mechanism analysis and the planet measurement that closed the
round-4 mitigation menu.

Europe simple phase breakdown (commit `b95e5ab`):
- Node classify: 13s, Node write: 11s
- Way classify: 6s, Way write: 40s
- Rel classify: 13s, Rel write: 2s

Historical: sorted pass1 optimization (commit `37b7c19`) impact on simple:
Denmark -14% (2625→2259ms), Japan -8% (12,619→11,643ms). Single-pass
classification on sorted input eliminates the second file read. Superseded
by the parallel classify + raw frame passthrough architecture.

## Tags-filter

Two-pass architecture: pass 1 classifies blobs in parallel (parallel
classification + lightweight scanner), closure + way dep scans also
parallelized via `parallel_classify_phase`, pass 2 writes matching
elements.

| Dataset | Sequential (old) | Two-pass (pass 1 only) | Two-pass (all parallel) | Commit |
|---------|-----------------|------------------------|------------------------|--------|
| Europe (33.6 GB) | 363s | 158s | **107.5s** (-70%) | latest |

Previously OOM with pipelined reader. Sequential fix (commit `2a8a649`)
brought it to 363s. Parallel classification for pass 1 brought it to
158s. Parallelizing closure + way dep scans brings the total to 107.5s.
Full journey: 366.7s → 107.5s (3.4x total improvement).

## Pipeline end-to-end

Bootstrap (one-time): `cat` → `add-locations-to-ways` → enriched PBF.
Steady state: `apply-changes --locations-on-ways` (daily diffs).

### Planet bootstrap (plantasjen, commit `69a127f`)

| Step | Time | Output |
|------|------|--------|
| cat (indexdata generation) | 497s (8m) | 87.7 GB |
| add-locations-to-ways | 5773s (96m) | 88.4 GB |
| **Total bootstrap** | **~104m** | — |

### Europe bootstrap (plantasjen, commit `69a127f`)

| Step | Time | Output |
|------|------|--------|
| cat (indexdata, `--type` filtered) | 112s | 33.6 GB |
| add-locations-to-ways | 2565s (43m) | — |
| **Total bootstrap** | **~45m** | — |

## build-geocode-index

Reverse geocoding index build. 4-pass pipeline: nodes (address points + dense
node index), ways (streets, buildings, interpolation), relations (admin boundary
assembly + simplification), S2 cell assignment (fine level 17 + coarse level 14).

| Dataset | PBF size | Time | Index size | Addr points | Streets | Admin | Commit |
|---------|----------|------|------------|-------------|---------|-------|--------|
| Denmark | 465 MB | **7.1s** | 172 MB | 2.6M | 314K | 2K | `f42da6e` |
| Japan | 2.4 GB | **26.7s** | — | — | — | — | `c33e8cc` |
| Germany | 4.5 GB | **1813s** (30m) | ~1.8 GB | 19.8M | 3.3M | 43K | `ed34092` |

### Japan sidecar profile (commit `5776b67`, plantasjen, --bench --sidecar)

| Phase | Duration | Peak RSS | Peak Anon | Disk Read | Disk Write | Majflt |
|-------|----------|----------|-----------|-----------|------------|--------|
| Pass 1 (relations) | 0.9s | 9 MB | 5 MB | 2.3 GB | 0 | 0 |
| Pass 2 (nodes+ways) | 55-60s | **19 GB** | **325 MB** | — | — | 1.3M (plateau, no thrash) |
| Pass 3 (S2 cells) | 1.9s | 352 MB | 273 MB | — | 539 MB | — |

Sequential reader (commit `5776b67`) keeps anon bounded at 325 MB — no
PrimitiveBlock cross-thread retention. The 19 GB peak RSS is the DenseMmapIndex
mmap (file-backed, fits in RAM at Japan scale). At Europe/planet scale this
mmap would thrash (same as dense ALTW).

Denmark: 0 interpolation ways (Scandinavian precise addressing). Germany: 78
interpolation ways with `addr:interpolation` + `addr:street`, 71/78 resolved.

### Optimization arc (Denmark, plantasjen)

| Commit | Change | Time | Cumulative |
|--------|--------|------|------------|
| `d27f17e` | Baseline (4 scans, sequential for_each) | 21.4s | — |
| `e7a12e6` | 3 scans (reorder: relations first) | 18.5s | -14% |
| `da4d939` | 2 scans (fused node+way, pipelined) | 10.9s | -49% |
| `60df011` | Zero-alloc cover_segment + parallel S2 cells | 10.4s | -51% |
| `398b1a4` | Block-pipelined, skip_metadata, tag-first way classification | 9.7s | -55% |

### Germany RSS profile (commit `3449db2`, plantasjen, hotpath)

588s total, 3.6 GB peak RSS. Per-phase memory:

| Phase | RSS | Wall time | Notes |
|---|---|---|---|
| After pass 1 (relations) | 223 MB | 1.8s | admin_relations + IdSetDense |
| After pass 2 scan (nodes+ways) | **17.6 GB** | 572s | Dense node index mmap dominates |
| After pass 2 drop (node index freed) | 168 MB | — | Pages evicted, data Vecs are modest |
| After ring assembly | 428 MB | +12.7s | + admin polygons (43K) |
| After interpolation resolution | 955 MB | +4.4s | + transient spatial index |
| After cell assignment | **3.7 GB** | +10s | All cell entry Vecs materialized |

Pipeline (`run_pipeline`) takes 556s / 94% — Germany is I/O + decompress bound
at this scale. Main thread CPU averages 32% (waiting on pipeline).

Key observations for planet-scale planning:
- Dense node index is the RSS peak (17.6 GB). Planet would push to ~30+ GB.
  Referenced-node-only index (pass 1.5 in planet spec) would cut this to ~10 GB.
- Cell entry Vecs are the second peak (3.7 GB). Planet estimate: ~19 GB.
  Bucketed cell assignment (planet spec) eliminates this.
- Data Vecs (streets, addr, interp, strings) are only ~168 MB after node index
  drops. Streaming to output files would reduce this further but is not the
  bottleneck at Germany scale.

### Comparison with traccar-geocoder

No directly comparable data — different hardware, different format, different
build architecture (traccar uses C++ with libosmium, single-threaded, all data
in RAM). Numbers from the HN thread (2026-03-21):

| Dataset | traccar-geocoder | pbfhogg | Notes |
|---------|-----------------|---------|-------|
| Australia/Oceania (~1.1 GB) | ~15 min (KomoD) | — | Not tested |
| Germany (4.5 GB) | — | **9.8 min** | After optimization (was 30 min) |
| Planet (~87 GB) | 8-10 hours (192 GB RAM) | — | Would OOM on 30 GB host |

Planet (validated): **1,346s (22.4 min), 17.8 GB RSS** (sidecar `6887288a`). Our index is larger due to segment-level indexing (6 bytes
vs 4 per entry), dual fine+coarse cell indices, and u64 node offsets. Our
builder currently holds all intermediate data in RAM — planet requires
streaming to temp files (not yet implemented).

traccar's index is more compact (18 GB planet) because it uses f32 coords,
u8 node counts, u32 offsets everywhere, whole-way indexing (4 bytes/entry),
and no coarse fallback. Our format trades size for query precision (segment-
level reads, i32 coords, wider offsets) and rural coverage (coarse index).

Query latency not yet benchmarked. Both architectures use the same algorithm
(S2 cell neighborhood + binary search + distance scoring on mmap'd data), so
sub-millisecond latency is expected.

## `--direct-io` impact summary

| Workload | Bottleneck | `--direct-io` effect |
|----------|------------|---------------------|
| Merge (NA, 18.8 GB) | I/O (concurrent read+write) | **-20%** (uring+none) |
| Merge (Germany, 4.5 GB) | Mixed | Neutral |
| Cat passthrough (planet) | Sequential I/O | +5% slower |
| ALTW (europe) | Memory latency (mmap faults) | +2% slower |

`--direct-io` only helps when page cache is a bottleneck (concurrent I/O on
files exceeding available RAM). Sequential reads and memory-bound workloads
are better served by buffered I/O with kernel readahead.
