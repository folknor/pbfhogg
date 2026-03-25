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

### Dense vs Sparse index (commit `52d6273`, plantasjen)

| Dataset | Dense | Sparse | Ratio |
|---------|-------|--------|-------|
| Denmark (465 MB) | **6.8s** | 14.1s | 2.1x |
| Europe (33.6 GB) | **2,565s** (43m) | 6,453s (107m) | **2.5x** |

Sparse is slower than dense at all tested scales. At Denmark scale the overhead
is pure CPU (sorting, hashing). At Europe scale the overhead ratio *increases*
(2.5x vs 2.1x) — the 16 GB on-disk values mmap thrashes just like the dense
mmap, but with additional sort+hash CPU cost on top.

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

### Comparison with traccar-geocoder

No directly comparable data — different hardware, different format, different
build architecture (traccar uses C++ with libosmium, single-threaded, all data
in RAM). Numbers from the HN thread (2026-03-21):

| Dataset | traccar-geocoder | pbfhogg | Notes |
|---------|-----------------|---------|-------|
| Australia/Oceania (~1.1 GB) | ~15 min (KomoD) | — | Not tested |
| Germany (4.5 GB) | — | **30 min** | Comparable scale to Aus/Oceania |
| Planet (~87 GB) | 8-10 hours (192 GB RAM) | — | Would OOM on 30 GB host |

Extrapolated planet: ~19 × 30 min = ~9.5 hours build time, ~34 GB index (vs
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
