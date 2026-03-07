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

## CLI commands

### Denmark (487 MB indexed, 59M elements)

Commit `23862d1` (full suite), plantasjen.

| Command | Time |
|---------|------|
| sort (sorted, indexdata) | 144 ms |
| cat --type relation | 217 ms |
| tags-filter highway=primary | 240 ms |
| inspect-tags --type way | 344 ms |
| getid | 528 ms |
| tags-filter amenity=* | 583 ms |
| cat --type way | 1.11s |
| inspect-nodes | 1.74s |
| inspect-tags | 1.80s |
| tags-filter two-pass | 2.53s |
| getid --invert | 2.72s |
| extract --simple | 2.79s |
| extract --complete | 2.86s |
| extract --smart | 3.02s |
| fileinfo (inspect) | 3.58s |
| add-locations-to-ways | 5.47s |
| check --refs | 7.19s |

### Japan (2.4 GB indexed, 344M elements)

| Command | Time |
|---------|------|
| cat --type relation | 365 ms |
| cat --type way | 3.44s |
| extract --smart | 12.2s |
| tags-filter two-pass | 11.9s |
| add-locations-to-ways | 43.1s |

### Germany (4.7 GB indexed)

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
