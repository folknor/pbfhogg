# Performance

## Read throughput

Count all 59M elements in Denmark extract (461 MB), best of 3 runs, fat LTO (commit `90df51f`):

| Tool | Mode | Time | Notes |
|------|------|------|-------|
| **pbfhogg** | parallel | **0.31s** | `par_map_reduce` on all cores |
| osmpbf 0.3 | parallel | 0.53s | upstream crate, same API |
| **pbfhogg** | pipelined | **1.3s** | `for_each_pipelined`, preserves file order |
| Planetiler 0.10 | parallel | 2.0s | Java, `OsmInputFile` + thread pool |
| **pbfhogg** | sequential | 2.8s | `for_each` |
| **pbfhogg** | blobreader | 2.9s | `BlobReader` sequential decode |
| osmpbf 0.3 | sequential | 5.6s | upstream `for_each` |
| osmium 1.19 | cat to opl | 5.7s | `osmium cat -f opl -o /dev/null` |
| Planetiler 0.10 | sequential | 8.7s | Java, `OsmInputFile` single-threaded |

`par_map_reduce` is fastest when order does not matter. `for_each_pipelined` is the fastest ordered read and the production hot path.

## Write throughput

Decode all 59M elements then write through `BlockBuilder` + `PbfWriter` to `/dev/null` (commit `def80d9`):

| Compression | Sync | Pipelined | Notes |
|-------------|------|-----------|-------|
| none | 6.2s | 6.2s | decode + wire-format serialization floor |
| zstd:3 | 8.1s | **6.2s** | pipelined hides compression cost |
| zlib:6 | 14.5s | **6.3s** | 2.3x speedup from parallel compression |

With pipelined writes, all compression modes converge to ~6.2s - the decode + wire-format serialization floor. All element types are encoded directly to protobuf wire format using reusable scratch buffers (no per-element allocation, no external protobuf dependencies).

## CLI command benchmarks

Denmark (487 MB, 59M elements, commit `6fc1283`, osmium from `23862d1`):

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| inspect (indexdata) | **0.1s** | -- | index-only fast path |
| sort (sorted, indexdata) | **0.7s** | 11.6s | **17x** |
| apply-changes (indexdata + zlib) | **0.6s** | 7.2s | **12x** |
| tags-filter w/highway=primary -R | **0.2s** | 0.56s | **2.8x** |
| tags-filter amenity=restaurant -R | **0.5s** | 1.19s | **2.4x** |
| cat --type way (raw passthrough) | **0.24s** | 2.22s | **9.3x** |
| inspect tags --type way (indexdata) | **0.4s** | 0.59s | **1.5x** |
| getid (9 elements) | **0.6s** | 0.83s | **1.4x** |
| add-locations-to-ways (sparse) | **9.9s** | 12.1s | **1.2x** |
| add-locations-to-ways (external) | **9.7s** | 12.1s | **1.2x** |

The largest speedups come from blob passthrough (sort, apply-changes, cat --type) where pbfhogg avoids decompressing and re-compressing unmodified blobs entirely.

## Extract benchmarks

Japan (2.4 GB, 344M elements, Tokyo bbox):

| Strategy | pbfhogg | osmium | ratio |
|----------|---------|--------|-------|
| simple | **4.4s** | 7.2s | **1.6x faster** |
| complete-ways | **4.4s** | 11.0s | **2.5x faster** |
| smart | **5.2s** | 13.4s | **2.6x faster** |

Simple extract uses a 3-phase barrier pipeline with parallel classification and raw frame passthrough. Complete-ways and smart use multi-pass parallel pread classification. Spatial blob filtering skips decompression of node blobs outside the extract region when indexdata is present.

## Apply-changes at scale

Single-pass 4-phase batch pipeline with O(log n) inline upsert assignment, reader thread read-ahead, and passthrough coalescing (commit `a6ebbfe`):

| Dataset | Config | Time | vs osmium |
|---------|--------|------|-----------|
| Japan (2.4 GB, 43K diff) | indexdata + zlib | **3.0s** | **15x** faster |
| Germany (4.5 GB, 146K diff) | buffered + zlib | **5.3s** | -- |
| Germany (4.5 GB, 146K diff) | buffered + none | **3.4s** | -- |
| N. America (18.8 GB, 645K diff) | buffered + zlib | **17.3s** | -- |
| N. America (18.8 GB, 645K diff) | buffered + none | **14.9s** | -- |
| N. America (18.8 GB, 645K diff) | io_uring + zlib | **15.2s** | -- |
| N. America (18.8 GB, 645K diff) | io_uring + none | **11.9s** | -- |

At Japan scale, osmium takes 36.6s for the same operation. pbfhogg passes ~92% of blobs through as raw bytes without decompression, using blob-level indexdata for O(1) classification. RSS stays under 600 MB even at North America scale (18.8 GB).

## Apply-changes with LocationsOnWays

Denmark (501 MB with LocationsOnWays, daily diff, commit `e7bbfa2`):

| Pipeline | pbfhogg | osmium | speedup |
|----------|---------|--------|---------|
| apply-changes `--locations-on-ways` | **3.9s** | 8.3s | **2.1x** |
| apply-changes + ALTW (separate) | 2.7s + 6.5s = 9.2s | 4.3s + 9.5s = 13.8s | -- |

The `--locations-on-ways` flag replaces a two-step pipeline with a single command.

## Planet-scale pipeline

The full production pipeline on the planet (87 GB, ~3.4B elements) runs on a 30 GB machine:

| Step | Command | Time | Peak memory |
|------|---------|------|-------------|
| Generate indexdata | `cat` | ~8 min | minimal |
| Add way-node coordinates | `add-locations-to-ways --index-type external` | ~24 min | ~17 GB |
| Build geocode index | `build-geocode-index` | ~22 min | ~18 GB |
| Apply daily diff | `apply-changes` | ~13 min | ~1.8 GB |

Every command runs with bounded memory. No 128 GB server required.

## Performance tips

### Use indexed PBFs

Generate an indexed PBF once with `pbfhogg cat`, then use it for all subsequent operations. Commands on indexed PBFs skip decompression of irrelevant blobs entirely, which is where the largest speedups come from.

### Choose the right ALTW index type

`add-locations-to-ways` supports two index strategies (plus `auto`):

| Type | Best for | Trade-off |
|------|----------|-----------|
| `sparse` (default) | Small to europe scale | Rank-indexed flat mmap (~8 bytes per referenced node); needs `referenced_count * 8` bytes of temp disk |
| `external` | Planet-scale, memory-constrained hosts | Bounded memory, all sequential I/O; needs sorted input + indexdata + ~256 GB temp disk at planet |
| `auto` | Recommended default | scale-aware: sparse unless the input is sorted+indexed and the estimated node store exceeds ~80% of available RAM |

At planet scale on a 30 GB machine, `external` is the only mode that survives. A previous `dense` mode was removed: sparse rank-indexed flat dominates dense at every measured scale.

`auto`'s threshold is computed at runtime from per-blob indexdata node counts, so the same file can route to `sparse` on one host and `external` on another. Below the threshold, `external`'s fixed scratch round trips cost more than they save - denmark measured sparse 5.8s vs external 12.3s, and north-america favors sparse by 26%.

### O_DIRECT for planet-scale I/O

Planet-scale operations read and write 80 GB+, polluting the entire page cache. The `--direct-io` flag bypasses the page cache entirely. Wall time is typically unchanged at country scale (CPU-bound) - the benefit is cache hygiene at planet scale and avoiding eviction of useful data from co-resident processes.

O_DIRECT wins for concurrent read/write patterns (merge). For sequential single-file passthrough (`cat`), buffered I/O is actually faster because the page cache prefetch helps.

### io_uring for large writes

The `--io-uring` flag replaces the synchronous writer thread with io_uring `WriteFixed`. At North America scale (18.8 GB), io_uring + `--compression none` is 20% faster than buffered writes (11.9s vs 14.9s). Below ~4 GB input size, buffered writes keep up.

### Compression choice

With pipelined writes (the production path), compression is dispatched to rayon and all modes converge to the decode + serialization floor. The choice mainly affects file size and downstream read speed:

- `none` - fastest writes, largest files, ideal for intermediate files or erofs storage
- `zlib` - standard PBF compression, compatible with all tools
- `zstd` - better ratio and faster decompression, but not all consumers support it yet

## System

All benchmarks measured on plantasjen: AMD Ryzen 9 5900X (12c/24t), 32 GB DDR4, NVMe SSD (input/output) + HDD (build artifacts), Linux 6.18. Measured with `brokkr bench`, cross-validated with `brokkr verify`.
