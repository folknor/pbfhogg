# External join optimization — Europe scale

## Setup
- Europe PBF: 32.4 GB, 4.69B way-node refs, 256 buckets
- Host: 32 GB RAM, ~25 GB free (plantasjen)
- External join: 4 stages (way pass, node join, slot reorder, assembly)

## The four stages

### Stage 1: Way pass
Scan all way blobs via pipelined reader. For each way, emit (node_id, slot_pos)
COO pairs into 256 node buckets partitioned by high bits of node_id. Output:
56 GB of bucket files on disk. BufWriter per bucket (256 KB buffers).

### Stage 2: Node join
Read all PBF node blobs. For each node, merge-join with the matching bucket's
sorted COO pairs. Emit resolved (slot_pos, lat, lon) entries into 256 slot
buckets partitioned by high bits of slot_pos.

### Stage 3: Slot reorder
Read slot buckets in order. For each bucket, scatter entries by position into
a dense buffer, then write the buffer sequentially to the coord_slots file.

### Stage 4: Assembly
Read the full PBF. For each element, look up way-node coordinates from
coord_slots. Write enriched PBF with node locations embedded in ways.

## OOM investigation timeline

### Phase 1: Assumed page cache problem

Initial RSS logging showed stage 2 hitting 21+ GB RSS. Assumed page cache.

**Attempted fixes (all failed to prevent stage 2 OOM):**

| Fix | Effect | Why it failed |
|-----|--------|---------------|
| Periodic flush+fadvise(DONTNEED) on bucket writes | No RSS change | fadvise only evicts clean pages; dirty pages survive |
| sync_data() before fadvise | Works (73 MB post-finish) | 4.4x throughput penalty (108s → 474s) |
| BlobReader fadvise after each blob read | Stage 1 post-finish: 46 MB | Stage 2 still OOM — freed read pages filled by write dirty pages |
| sync_file_range(SYNC_FILE_RANGE_WRITE) async writeback | Zero effect | Async writeback not completing before next cycle |
| O_DIRECT for bucket writes (DirectWriter) | Zero effect | Same linear RSS growth |

### Phase 2: RssAnon/RssFile breakdown

Added `/proc/self/status` parsing. **file=4MB throughout.** ALL 24+ GB was
anonymous heap. Every page cache fix was targeting the wrong problem.

### Phase 3: Allocator theory (disproved)

| Test | Stage 2 RSS growth | Result |
|------|-------------------|--------|
| `MALLOC_ARENA_MAX=2` | Same linear growth | No help |
| `MALLOC_ARENA_MAX=1` | Same (delayed start, reuses stage 1 arena) | No help |
| jemalloc (`--features jemalloc`) | Same linear growth | No help |

NOT allocator arena retention. Three different allocator configs produce
identical growth.

### Phase 4: Binary search for the leak

| Test | RSS | Conclusion |
|------|-----|------------|
| `continue` before element loop (skip everything) | 383 MB flat | Pipeline NOT leaking |
| Iterate elements + extract id/lat/lon, skip merge-join | 478 MB → 25168 MB, plateaued | Leak in element iteration |
| Full merge-join, disable writes | Same growth | Writes not the problem |
| Buffer reuse in load_coo_bucket | Same growth | Bucket loading not the problem |
| DecompressPool full-drops counter | 52 drops / 464K blocks | Pool NOT the problem |

### Root cause

PrimitiveBlock construction allocates per-block on rayon decode threads:
- `WireStringTable::entries: Box<[(u32, u32)]>` (~100-1000 entries)
- `WireBlock::group_ranges: Box<[(u32, u32)]>` (~1-4 entries)
- `into_boxed_slice()` reallocs (Vec→Box, freeing Vec overallocation)

Allocated on rayon threads, freed on consumer thread (cross-thread).
Neither glibc nor jemalloc returns the physical pages to the OS fast enough.
464K blocks × ~54 KB retained/block = ~25 GB peak before plateau.

The plateau proves it's not a logical leak — the allocator eventually reuses
freed memory. But the full merge-join OOMs at 27 GB because it adds ~2 GB
of live data on top.

## Current implementation (commit `cf350a9`)

### Stage 1: Way pass — pipelined reader + BufWriter buckets
- **Time:** 81s (Europe)
- **RSS:** ~11 GB peak (write cache), 114 MB post-finish
- **Implementation:** Standard pipelined `ElementReader` with `BlobFilter::only_ways()`.
  BufWriter per bucket. `sync_data+fadvise` in `finish()` for stage boundary cleanup.

**Tested permutations:**

| Variant | Time | RSS peak | Notes |
|---------|------|----------|-------|
| BufWriter (current) | 81s | ~11 GB | Dirty write pages in kernel cache |
| DirectWriter (O_DIRECT) | 108s | ~11 GB | Same RSS (was heap, not page cache) |

DirectWriter was slower and didn't help RSS. Reverted to BufWriter.

**Untested permutations:**
- `--direct-io` for PBF reads (may help RSS if combined with BlobReader fadvise)
- jemalloc (throughput comparison, not memory — memory is bounded by finish())

### Stage 2: Node join — sequential node-only scanner
- **Time:** 327s (Europe), 6s (Denmark)
- **RSS:** 1405 MB stable through 522K blocks, 114 MB post-finish
- **Implementation:** Sequential `BlobReader` + `decompress_raw()` + inline wire
  format parsing. No `PrimitiveBlock`, no string table, no `WireBlock`, no
  `Box<[...]>` allocations. Delta-decodes id/lat/lon from `PackedSint64Iter`.

**Tested permutations:**

| Variant | Time | RSS | Notes |
|---------|------|-----|-------|
| Pipelined PrimitiveBlock (original) | — | OOM at 27 GB | Cross-thread alloc/free |
| Sequential node-only scanner (current) | 327s | 1405 MB | Single-threaded zlib decompression |
| Pipelined node-only scanner | — | OOM at 23 GB | DecompressPool cross-thread pattern |
| Element iteration only (no merge-join) | — | 25 GB plateau | Proves it's PrimitiveBlock, not merge-join |

**Correctness:** Denmark output verified identical to dense index (0 diffs).

**Bottleneck analysis:** 327s is single-threaded zlib decompression of ~25 GB
of compressed node data at ~80 MB/s. At the CPU ceiling — no algorithmic
improvement possible, only parallelism.

**Proposed improvements (from reviewer consensus):**

| Approach | Description | Expected | Risk |
|----------|-------------|----------|------|
| A: IO overlap | IO thread reads, consumer decompresses+parses | ~5-15% gain | Low |
| B: Parallel tuples | Rayon threads decompress + extract (id,lat,lon) tuples, send tuples through channel | 4-6x (55-80s) | Medium — new channel/ordering boundary |
| C: Consumer-side pool | Rayon sends compressed blobs, consumer decompresses | Same as sequential | N/A |
| D: Accept 327s | Move to stage 4 | — | — |

**Reviewer consensus:** Fix stage 4 first. The pattern that fixes stage 4
(sequential read + batch parallel encode) likely transfers to stage 2.

### Stage 3: Slot reorder — scatter buffer
- **Time:** 72s (Europe), 810ms (Denmark)
- **RSS:** 114 MB stable
- **Implementation:** For each bucket, allocate zeroed buffer covering the
  bucket's slot range (~146 MB at Europe scale). Scatter entries by position
  (no sort). Write entire buffer via `write_all`. Buffer reused across buckets.

**Previous implementation:** 4.69B individual `pwrite64` calls (8 bytes each).
~938s of pure syscall overhead. **15x speedup** from scatter buffer.

**Tested permutations:**

| Variant | Time | Notes |
|---------|------|-------|
| pwrite per entry (original) | 1079s | 4.69B syscalls |
| Scatter buffer (current) | 72s | 256 write_all calls, no sort |

**Untested permutations:**
- Sequential BufWriter with sentinel fill (approach 1 from reviewers)
- mmap coord_slots + memcpy
- Parallel bucket processing (rayon)

### Stage 4: Assembly — DISABLED (OOM)
- **Status:** Temporarily disabled. OOM killed at Europe scale.
- **Root cause:** Same PrimitiveBlock cross-thread alloc/free as stage 2.
  Standard pipelined reader (`into_blocks_pipelined`) + full element iteration
  for assembly causes 25+ GB heap retention. With IdSetDense (1.4 GB) on top,
  exceeds 32 GB host.

**Proposed fix (from reviewer consensus):**

Sequential read + batch parallel encode:
1. Read blocks sequentially (no cross-thread buffer ownership during read)
2. Accumulate a batch of N blocks
3. `par_iter` over the batch for BlockBuilder encoding
4. Write OwnedBlocks to PbfWriter

This is the same `assemble_batch` pattern already in the code, but fed by a
sequential reader instead of the pipelined one. Estimated: ~250-350s.

**Alternative:** `decode_threads(1)` on the pipelined reader — limits to
one decode thread, reducing cross-thread churn. Still uses PrimitiveBlock.
May not fully fix the retention.

## Summary of timings

### Europe (32.4 GB, commit `cf350a9`, plantasjen)

| Stage | Original | Current | Speedup | Status |
|-------|----------|---------|---------|--------|
| Stage 1 (way pass) | 108s | 81s | 1.3x | Done |
| Stage 2 (node join) | OOM | 327s | N/A (was broken) | Done (sequential) |
| Stage 3 (slot reorder) | 1079s | 72s | 15x | Done |
| Stage 4 (assembly) | OOM | disabled | — | **Blocked** |
| **Total 1-3** | **1502s** | **480s** | **3.1x** | |
| **Estimated 1-4** | — | **~730-830s** | — | After stage 4 fix |

### Denmark (465 MB, commit `cf350a9`, plantasjen)

| Stage | Time | RSS peak |
|-------|------|----------|
| Stage 1 | 3.6s | 45 MB |
| Stage 2 | 6.5s | 148 MB |
| Stage 3 | 0.8s | 51 MB |
| **Total 1-3** | **11s** | |

### Historical comparison

| Version | Denmark | Europe | Commit |
|---------|---------|--------|--------|
| Original (256× re-read) | 302s | — | `034422c` |
| Single-pass merge | 25s | 2,060s (34m) | `a334c72` |
| **Node-only scanner + scatter** | **11s** | **480s (8m, stages 1-3)** | `cf350a9` |

## What we shipped

- **BlobReader fadvise(DONTNEED)** — commit `4ab6976`. General infrastructure.
  Evicts page cache pages behind read head after each blob. Benefits all
  single-pass forward scans. Stage 2 RSS 383 MB with pipeline-only drain.
- **Deferred IdSetDense** — moved from before stage 1 to before stage 4.
  Saves 1.4 GB during stages 1-3.
- **Node-only wire-format scanner** — bypasses PrimitiveBlock for stage 2.
  Zero per-block heap allocations. 1.4 GB stable RSS at Europe scale.
- **Scatter buffer for stage 3** — eliminates sort and 4.69B pwrite calls.
  15x speedup.
- **Buffer reuse in load_coo_bucket_into** — clear+refill instead of new Vecs.
  Doesn't fix the OOM (root cause was PrimitiveBlock, not buckets) but good
  practice.
- **sync_data+fadvise in BucketWriters::finish()** — clean stage boundary.
- **Blob::decompress_raw() and decompress_pooled()** — decompression without
  PrimitiveBlock construction.
- **DecompressPool::pool_full_drops counter** — diagnostic.
- **RSS logging with RssAnon/RssFile breakdown** — essential for diagnosis.
- **Hotpath annotations** on all external join functions.

## Transferable insights

- **RssAnon/RssFile breakdown** — essential for diagnosing memory issues.
  Without this, we chased page cache for hours when the problem was heap.
- **PrimitiveBlock cross-thread alloc/free** — the pipelined reader's
  PrimitiveBlock construction causes ~54 KB/block of heap retention from
  cross-thread alloc/free of WireStringTable entries and group_ranges.
  This affects ANY consumer that uses `into_blocks_pipelined` with element
  iteration at scale (464K+ blocks). The node-only scanner pattern
  (decompress + inline wire parse) avoids this entirely.
- **fadvise(DONTNEED) only evicts clean pages** — dirty pages from write()
  survive DONTNEED. sync_data() makes them clean but is expensive.
  sync_file_range(SYNC_FILE_RANGE_WRITE) is async but didn't help in practice.
- **O_DIRECT on reads paradox** — freeing read pages via O_DIRECT or fadvise
  lets the kernel fill that space with dirty write pages, potentially making
  RSS worse. Must control both read and write page cache together.
- **Dense scatter buffer** — any radix-bucketed workflow where bucket
  partitioning defines output order. Eliminates sort when position is
  computable from the entry. 15x speedup at Europe scale.
- **Node-only scanner** — any command that only needs id/lat/lon from node
  blobs. Bypasses PrimitiveBlock entirely. Applicable to geocode builder,
  extract pass 1, and any future node-only command.
- **BlobReader fadvise(DONTNEED)** — general infrastructure improvement for
  all single-pass forward scans.

## Europe end-to-end result (commit `ee9b19f`, plantasjen)

| Stage | Time | RSS post-finish |
|-------|------|-----------------|
| Stage 1 (way pass) | 82s | 74 MB |
| Stage 2 (node join) | 331s | 74 MB |
| Stage 3 (slot reorder) | 73s | 74 MB |
| Relation scan | — | 1342 MB |
| Stage 4 (assembly) | 392s | 10587 MB |
| **Total** | **901s (15 min)** | |

Output: 3.7B nodes read, 149M written, 454M ways, 8.2M relations, 0 missing.
DecompressPool: 103 drops (stage 1), 12 drops (stage 4).

Comparison: dense at Europe scale = 2,565s (43 min). External = **2.8x faster.**
Previous external (single-pass merge, `a334c72`) = 2,060s (34 min). **2.3x faster.**

## Next steps

1. ~~Fix stage 4~~ — done (commit `2873919`, sequential reader)
2. ~~Full end-to-end Europe measurement~~ — done (901s, commit `ee9b19f`)
3. **Stage 2 parallelism** — approach B (thread-local decompress, send tuples).
   Stage 2 is 37% of total time (331s/901s). Parallel decompression could
   bring it to ~55-80s, total to ~620-650s (~10 min).
4. **Stage 4 optimization** — 44% of total time (392s/901s). Currently
   sequential decode. Similar parallel approach as stage 2, but needs full
   PrimitiveBlock (not node-only). RSS peaked at 10.6 GB — room for more
   in-flight blocks if parallelized carefully.
5. **Planet benchmark** — full pipeline on 87.7 GB PBF.
