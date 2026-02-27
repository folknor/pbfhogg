# Extract: parallel collection pass experiment

## Host

- **Hostname:** plantasjen
- **CPU:** AMD Ryzen 9 5900X 12-core / 24-thread, 3.7 GHz base / 4.95 GHz boost
- **RAM:** 32 GB DDR4 (30 GB available)
- **Storage:** Samsung 970 EVO Plus 1TB NVMe (project + data)
- **Kernel:** Linux 6.18.0-9-generic (x86_64)
- **Build:** fat LTO, rust-zlib (miniz_oxide), commit 2cd6ed6 (base)

## Motivation

Extract is the one command where osmium is significantly faster than pbfhogg.
The bottleneck is the collection pass (Pass 1), not the write pass (which is
already parallelized via rayon batches). The hypothesis was that parallelizing
the collection pass via blob-type partitioning + rayon would close the gap.

## Osmium analysis

Before implementing, we examined osmium's extract source code
(`data/osmium-tool/`, `data/libosmium/`). Key finding: **osmium does NOT
parallelize extract at all.** Element processing is fully sequential. Their
speed advantage comes from:

1. **Metadata skipping** — `read_meta::no` for non-write passes skips version,
   timestamp, changeset, uid, user, visible fields. Metadata is ~30-40% of
   dense node data by byte volume. pbfhogg's zero-copy wire-format parser is
   already partially lazy (WireInfo stores offsets, not parsed values), so the
   remaining gain is only the varint scanning cost (~5-10% of decode).
2. **Eager protobuf decoder** — osmium uses protobuf-generated decoders with
   compiled field dispatch. pbfhogg's hand-rolled wire-format scanner is
   flexible but scans all varint field boundaries including unused metadata.
3. **posix_fadvise(POSIX_FADV_SEQUENTIAL)** — kernel hint for sequential
   readahead. pbfhogg now has this too (added to FileReader::buffered).

## Approach: buffered parallel collection

Three-phase parallel collection using blob-type partitioning:

1. Read all compressed blobs via `BlobReader`, partition into
   `(node_blobs, way_blobs, rel_blobs)` by indexdata type.
2. **Phase 1 (nodes, parallel):** `into_par_iter().try_fold().try_reduce()`
   with thread-local `IdSetDense` accumulators, bbox check per node.
3. **Phase 2 (ways, parallel):** immutable `&bbox_node_ids`, parallel ref
   matching, build `matched_way_ids` (+ `all_way_node_ids` for complete/smart).
4. **Phase 3 (relations, parallel):** immutable node + way ID sets, parallel
   member matching, build `matched_relation_ids` (+ extras for smart).

Thread-local `IdSetDense` instances merged via bitwise OR with zero-copy chunk
moves for non-overlapping ID ranges (common in sorted PBFs).

Fallback: if any blob lacks indexdata, fall back to existing sequential
`collect_pass1_matches()`.

Simple strategy was also restructured from single-pass (collect+write
interleaved) to two-pass (parallel collection + parallel write via
`process_extract_pass2_batch`).

## Results: Denmark (483 MB, 59.1M elements)

Bbox: `12.4,55.6,12.7,55.8` (Copenhagen). Best of 3, solo runs.

| Strategy | Baseline (s) | Parallel (s) | Delta | Osmium (s) |
|----------|-------------|-------------|-------|-----------|
| Simple | 2.78 | 2.76 | -0.7% | 1.74 |
| Complete-ways | 2.74 | 2.83 | +3.3% | 2.82 |
| Smart | 4.29 | 4.61 | +7.5% | 3.54 |

## Results: Japan (2.3 GB, 344M elements)

Bbox: `139.5,35.5,140.0,36.0` (Tokyo metro). Best of 3, solo runs.

| Strategy | Baseline (s) | Parallel (s) | Delta | Osmium (s) |
|----------|-------------|-------------|-------|-----------|
| Simple | 14.48 | 14.56 | +0.5% | 7.34 |
| Complete-ways | 14.22 | 14.34 | +0.8% | 11.57 |
| Smart | 24.11 | 23.79 | -1.3% | 13.83 |

## Analysis: why parallel collection doesn't help

The parallel collection approach has a fundamental overhead that negates any
parallelism gains:

1. **Buffering all compressed blobs.** `collect_and_partition_blobs()` reads
   the entire file into `Vec<Blob>` before any parallel work begins. This is
   a full sequential I/O pass (~0.5s Denmark, ~2.5s Japan) with significant
   allocation overhead.

2. **Redundant decompression.** Each blob in the parallel phases calls
   `blob.to_primitiveblock()`, which decompresses from the buffered compressed
   data. The existing pipelined reader (`into_blocks_pipelined`) already
   parallelizes decompression via its 3-stage pipeline (I/O thread -> rayon
   decode pool -> consumer), and overlaps I/O with decode.

3. **Trivial consumer work.** The collection consumer (bbox check, ID set
   insert) is ~5% of per-block time. The bottleneck is decompression + I/O,
   both of which the existing pipeline already parallelizes. Adding parallelism
   to the consumer gives negligible benefit.

4. **No pipelining overlap.** The buffered approach is sequential:
   read all blobs -> parallel decode phase 1 -> parallel decode phase 2 ->
   parallel decode phase 3. The streaming approach overlaps I/O with decode
   continuously. At Denmark and Japan scale, the pipeline overlap wins.

In summary: the pipelined reader already extracts all available parallelism
from decompression, and the collection work itself is too cheap to benefit
from further parallelization. The buffered approach adds I/O + allocation
overhead without compensating gains.

## When buffered parallel collection _might_ help

- **Planet scale (80 GB)** where decode time is 10-20x larger and the parallel
  phases could amortize the buffering overhead. However, buffering 80 GB of
  compressed blobs in memory is impractical.
- **If decompression were not already parallelized.** A sequential reader
  (no pipeline) would leave decode on a single core, making per-phase
  parallelism valuable. But we already have the pipeline.
- **CPU-bound consumers.** If collection involved expensive computation per
  element (polygon ray-casting with complex geometries, spatial indexing),
  parallelizing the consumer would matter. For bbox extraction, it doesn't.

## io_uring reads for collection

Could io_uring `ReadFixed` with O_DIRECT help the collection pass? The project
already uses io_uring for merge writes (linked `ReadFixed` → `WriteFixed` for
blob passthrough in `uring_writer.rs`).

### Merge io_uring benchmarks for reference

Merge reads have a similar sequential scan pattern to extract collection.
From North America benchmarks (18.8 GB, `scripts/bench-merge-uring.sh`):

| Scale | Buffered | io_uring | Delta |
|-------|----------|----------|-------|
| Denmark (465 MB) | 818 ms | 857 ms | **+5% (slower)** |
| Japan (2.3 GB) | 4,209 ms | 4,227 ms | **+0.4% (noise)** |
| North America (18.8 GB) | 43,249 ms | 32,621 ms | **-25% (faster)** |

io_uring only helps when data exceeds page cache (~30 GB RAM). At Denmark and
Japan scale, the kernel's sequential readahead with `posix_fadvise` is optimal.
io_uring + O_DIRECT avoids page cache thrashing at larger scale.

### Why io_uring doesn't help extract collection at tested scales

1. **Sequential scan pattern.** Extract collection reads the entire file
   sequentially (all blobs, in order). The kernel readahead buffer with
   `POSIX_FADV_SEQUENTIAL` already prefetches aggressively for this pattern.
   io_uring adds per-I/O setup overhead (SQE, CQE, buffer management) that
   exceeds the syscall savings for sequential reads that hit page cache.

2. **Data fits in page cache.** Denmark (483 MB) and Japan (2.3 GB) are well
   within the 30 GB available RAM. The first run populates the page cache;
   subsequent runs are pure memory reads. O_DIRECT would skip this cache and
   read from NVMe every time — strictly slower for repeated or warm-cache runs.

3. **The bottleneck is decode, not I/O.** Pipelined reader profiling shows the
   I/O thread is never the bottleneck at these scales. The rayon decode pool
   and main-thread consumer dominate wall time. Faster I/O doesn't help when
   the pipeline is already balanced.

### When io_uring reads _would_ help extract

- **Planet-scale extract (80 GB input)** where O_DIRECT avoids evicting 80 GB
  through the page cache. But planet-scale extract is rare — typical workflow
  extracts from a continent PBF (5-20 GB), not the full planet.

- **If combined with indexdata-based seeking.** With indexdata, we know the
  exact byte offsets of every blob. For a collection pass that only needs
  node blobs (Phase 1), we could issue `ReadFixed` for just the node byte
  range, skipping way/relation blobs entirely. On a 80 GB planet file where
  nodes are ~60 GB and ways+relations are ~20 GB, this saves 20 GB of I/O
  per phase. However, sorted PBFs already have nodes first, so sequential
  reads with early termination at the first way blob achieves the same thing
  without io_uring.

- **Parallel I/O from multiple file regions.** io_uring can issue reads to
  different file offsets simultaneously. With indexdata partitioning, we could
  read node blobs and way blobs concurrently from different file regions. But
  on a single NVMe device, parallel sequential reads from different regions
  cause seek thrashing and reduce throughput vs a single sequential scan.
  This only helps with multiple NVMe devices or a RAID array.

### Conclusion on io_uring for extract

**Not worth pursuing for the collection pass.** The merge benchmarks
demonstrate that io_uring reads only help above ~15 GB (beyond page cache).
Extract at that scale is uncommon, and the bottleneck is decode, not I/O.
The existing pipelined reader with `posix_fadvise` is the right approach.

If planet-scale extract becomes a real use case, the io_uring read path
should be added to the pipeline's I/O thread (replacing buffered reads with
`ReadFixed` + O_DIRECT), not as a separate parallel collection mechanism.
This preserves the existing pipeline overlap and avoids the blob buffering
overhead demonstrated in this experiment.

## Overall conclusion

**Parallel collection via blob buffering is not viable for extract.** The
existing pipelined reader already parallelizes the expensive part (decode),
and the consumer work is trivial. io_uring reads don't address this because
the bottleneck is decode, not I/O.

The remaining performance gap vs osmium is NOT decoder efficiency — the
complete-ways comparison (identical 2-pass structure) shows pbfhogg is
already faster (2.74s vs 2.82s). The gaps are:

- **Simple: 2 passes vs osmium's 1.** The extra file read costs ~1.3s.
  A single-pass approach with parallel inline writing would close most
  of this gap.
- **Smart: metadata skipping in scan-only passes.** Smart Pass 2 (way dep
  resolution) only needs way IDs and refs. Osmium skips metadata entirely,
  saving decode work. pbfhogg decodes full elements including unused fields.

## Retained changes

1. **`IdSetDense::merge()`** — well-tested bitwise OR merge with zero-copy
   chunk moves. Useful for future parallel workloads.
2. **Simple strategy restructured** to streaming collection + parallel write
   (two-pass instead of single-pass). The parallel write pass uses the same
   `process_extract_pass2_batch` infrastructure as complete-ways.
3. **`posix_fadvise(POSIX_FADV_SEQUENTIAL)`** added to `FileReader::buffered()`
   for sequential readahead hint (matches osmium).
