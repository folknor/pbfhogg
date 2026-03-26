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

Germany (4.7 GB, 146K-change daily diff): rewrite fraction 18.4%.
North America (18.8 GB, 645K-change daily diff): 303K passthrough / 19.6K
rewritten blobs. All variants under 600 MB RSS.

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
| Denmark (465 MB) | **6.8s** | 14.1s | 25s | `a334c72` |
| Japan (2.4 GB) | **72s** | 72s | 143s | `a334c72` |
| Europe (33.6 GB) | 2,565s (43m) | 6,453s (107m) | **2,060s (34m)** | `0b5507f` |
| Planet (87.7 GB) | 5,773s (96m)* | — | ~90m (est.) | — |

*Planet with dense thrashes on 30 GB host (memory-latency-bound).

Dense is fastest when the working set fits in RAM. External uses <1 GB RAM
at any scale via bucketed sequential I/O (4-stage radix join pipeline).

**Crossover point**: between Japan (2.4 GB, dense 2x faster) and Europe
(33.6 GB, external 20% faster). At Europe scale, dense's mmap working set
(~16 GB) exceeds available RAM after page cache pressure from the 33.6 GB
input file, causing thrashing. External's sequential I/O stays bounded.

Planet extrapolation: external ~90 min (2.6× Europe) vs dense 96 min
(measured, thrashing). External should be comparable or faster at planet
scale while using <1 GB RSS vs dense's 16 GB mmap. See `notes/altw-partitioned.md`.

Sparse is slower than dense at all scales. At Europe scale the overhead
ratio *increases* (2.5x vs 2.1x) — the 16 GB on-disk values mmap thrashes
just like the dense mmap, with additional sort+hash CPU cost on top.

The external join was dramatically improved by a single-pass node merge
(commit `a334c72`): Denmark 302s → 25s (12x). The previous implementation
re-read ALL PBF node blobs 256 times (once per bucket); the new version
reads them exactly once.

See [altw-memory.md](altw-memory.md) for full analysis and next steps.

## CLI commands

Commit `aacbe80`, plantasjen. Best of 3 runs.

### Denmark (487 MB indexed, 59M elements)

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
| cat --type way | 614 ms |
| merge-changes | 107 ms |
| inspect-tags | 1.61s |
| inspect-nodes | 1.73s |
| check --ids | 1.87s |
| getid --invert | 1.87s |
| extract --simple | 2.48s |
| extract --complete | 2.40s |
| tags-filter two-pass | 2.62s |
| extract --smart | 2.65s |
| add-locations-to-ways | 5.59s |
| check --refs | 6.83s |
| time-filter | 9.39s |
| cat --dedupe | 22.4s |
| renumber | 22.3s |

### Japan (2.4 GB indexed, 344M elements)

| Command | Time |
|---------|------|
| inspect (indexdata) | 92 ms |
| tags-filter-osc | 169 ms |
| cat --type relation | 306 ms |
| tags-filter highway=primary | 840 ms |
| sort (sorted, indexdata) | 1.33s |
| merge-changes | 1.62s |
| getid | 1.94s |
| getparents | 2.06s |
| tags-filter amenity=* | 2.20s |
| inspect-tags --type way | 2.43s |
| apply-changes | 2.53s |
| cat --type way | 3.45s |
| inspect-tags | 4.82s |
| getid --invert | 8.55s |
| inspect-nodes | 9.14s |
| extract --simple | 9.36s |
| check --ids | 10.4s |
| extract --complete | 11.6s |
| extract --smart | 12.9s |
| tags-filter two-pass | 13.7s |
| check --refs | 38.7s |
| time-filter | 43.8s |
| add-locations-to-ways | 64.1s |
| diff | 72.2s |
| diff --format osc | 73.1s |
| cat --dedupe | 102.2s |
| renumber | 152.4s |

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
| cat --type way (indexdata) | **1.1s** | 2.22s | **2.0x** |
| add-locations-to-ways | **8.3s** | 12.6s | **1.5x** |
| check --refs | **4.8s** | 4.5s | 0.94x |

## Extract

Commit `1b10bfd`, plantasjen. Best of 3 runs, indexed PBFs.

| Dataset | Size | simple | complete | smart |
|---------|------|--------|----------|-------|
| Denmark | 487 MB | 2259 ms | 2399 ms | 2693 ms |
| Japan | 2.4 GB | 11,643 ms | 12,213 ms | 13,893 ms |

Denmark bbox `12.4,55.6,12.7,55.8`, Japan bbox `139.5,35.5,140.0,36.0`.

Sorted pass1 optimization (commit `37b7c19`) impact on simple strategy:
Denmark -14% (2625→2259ms), Japan -8% (12,619→11,643ms). Single-pass
classification on sorted input eliminates the second file read.

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

Commit `ed34092`, plantasjen.

| Dataset | PBF size | Time | Index size | Addr points | Streets | Admin |
|---------|----------|------|------------|-------------|---------|-------|
| Denmark | 465 MB | **20.8s** | 172 MB | 2.6M | 314K | 2K |
| Germany | 4.5 GB | **1813s** (30m) | ~1.8 GB | 19.8M | 3.3M | 43K |

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

Extrapolated planet: ~19 × 10 min = ~3.2 hours build time, ~34 GB index (vs
traccar's 18 GB). Our index is larger due to segment-level indexing (6 bytes
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
