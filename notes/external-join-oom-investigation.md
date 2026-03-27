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

### Europe end-to-end (32.4 GB, plantasjen)

| Stage | commit `ee9b19f` | commit `d272b49` | RSS post-finish |
|-------|-----------------|-----------------|-----------------|
| Stage 1 (way pass) | 82s | 82s | 74 MB |
| Stage 2 (node join) | 331s | **301s** | 74 MB |
| Stage 3 (slot reorder) | 73s | 73s | 74 MB |
| Relation scan | — | — | 1342 MB |
| Stage 4 (assembly) | 392s | 392s | 10587 MB |
| **Total** | **901s (15 min)** | **~871s (est.)** | |

Stage 2 improvement: skip non-node blobs + DecompressPool reuse (-30s, -9%).

### Denmark end-to-end (465 MB, commit `ee9b19f`, plantasjen)

| Stage | Time | RSS peak |
|-------|------|----------|
| Stage 1 | 2.7s | 40 MB |
| Stage 2 | 4.2s | 53 MB |
| Stage 3 | 0.6s | 58 MB |
| Stage 4 | 5.0s | 840 MB |
| **Total** | **14s** | |

### Historical comparison

| Version | Denmark | Europe | Commit |
|---------|---------|--------|--------|
| Original (256× re-read) | 302s | — | `034422c` |
| Single-pass merge | 25s | 2,060s (34m) | `a334c72` |
| Node-only scanner + scatter (stages 1-3) | 11s | 480s (8m) | `cf350a9` |
| End-to-end (all 4 stages) | 14s | 901s (15m) | `ee9b19f` |
| + blob skip + pool reuse | 14s | ~871s (est.) | `d272b49` |
| Full baseline (measured) | 14s | 930s | post-`d272b49` |
| **+ decode_threads(1) stage 4** | 14s | **~834s (est.)** | pending commit |

Dense ALTW at Europe scale: 2,565s (43 min). **External is ~3.1x faster.**

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
| Stage 2 (node join) | 331s → **301s** (commit `d272b49`) | 74 MB |
| Stage 3 (slot reorder) | 73s | 74 MB |
| Relation scan | — | 1342 MB |
| Stage 4 (assembly) | 392s → **383s** (decode_threads(1)) | 10587 MB → **1804 MB** |
| **Total** | **~834s (est.)** | |

Output: 3.7B nodes read, 149M written, 454M ways, 8.2M relations, 0 missing.
DecompressPool: 103 drops (stage 1), 12 drops (stage 4).

Comparison: dense at Europe scale = 2,565s (43 min). External = **2.8x faster.**
Previous external (single-pass merge, `a334c72`) = 2,060s (34 min). **2.3x faster.**

## Optimization matrix

Baseline: Europe end-to-end 901s (commit `ee9b19f`, plantasjen).

### Priority 1: Wasted work elimination

| ID | Stage | Approach | Expected | Risk | Effort |
|----|-------|----------|----------|------|--------|
| ~~P1a~~ | ~~Stage 2 (331s)~~ | ~~**Skip non-node blobs.** Check indexdata, skip non-node blobs before decompression.~~ | ~~15-20%~~ | ~~Done~~ | **Measured: 301s (-9%, -30s). commit `d272b49`** |
| P1b | Stage 4 (392s) | **Tagdata-based node blob skipping.** With `keep_untagged_nodes=false` (default), node blobs whose tagdata shows zero tag keys contain only untagged nodes → skip entirely. 96% of nodes are dropped in Europe. Could skip 90%+ of node blob decompression. | **Up to 75% of stage 4 node processing** | Medium — needs tagdata parsing | Medium |
| P1c | Stage 4 (392s) | **Relation blob passthrough.** Relation blobs don't need coordinate enrichment. With indexdata, skip decompression and pass through raw. ~600 blobs at Europe. | Small (~few seconds) | Low | Low |

### Priority 2: Alloc/decompress optimization

| ID | Stage | Approach | Expected | Risk | Effort |
|----|-------|----------|----------|------|--------|
| ~~P2a~~ | ~~Stage 2~~ | ~~Reusable decompress buffer (DecompressPool for single-thread buffer reuse).~~ | ~~5-10%~~ | ~~Done~~ | **Measured as part of P1a: combined -9%. commit `d272b49`** |
| P2b | Stage 2 | **Parallel tuples (approach B).** Rayon threads decompress + extract (id,lat,lon) into worker-owned Vecs, send tuples through channel. Thread-local decompress buffers + tuple buffers recycled back to workers (not dropped cross-thread). | 4-6x (55-80s) | Medium | Medium |
| P2c | Stage 4 | Same parallel pattern as P2b but for full elements. Workers decompress + extract element data, send compact work units. Heavy data dies on worker thread. | 3-4x (100-150s) | Higher | Medium-High |
| P2d | All | Faster zlib backend. `decompress_blob` is 53.5% of time. If `zlib-rs` has a faster mode or if a different backend exists. | Unknown | Low | Low |

### Priority 3: Micro-optimizations

| ID | Stage | Approach | Expected | Risk | Effort |
|----|-------|----------|----------|------|--------|
| P3a | Stages 1+2 | **Precompute range_size divisions.** `CooPair::node_bucket()` and `ResolvedEntry::slot_bucket()` recompute `div_ceil` per entry (4.69B calls each). Hoist to a local. | ~1-2s each | None | Trivial |
| P3b | Stage 2 | **Early exit on exhausted bucket.** When `pair_cursor == sorted_pairs.len()`, skip the compare path for remaining nodes until next bucket transition. | Small | None | Trivial |
| P3c | Stage 2 | **Eliminate data_buf intermediate.** Parse COO pairs directly from a BufReader (16 bytes at a time) instead of reading entire bucket file into memory. Eliminates ~290 MB data_buf. | ~5-10s | Low | Low |
| P3d | Stages 2+4 | `set_parse_tagdata(false)` on BlobReaders that don't need tag data. | Minimal | None | Trivial |

### Priority 4: Cross-cutting experiments

| ID | Experiment | Applies to | Expected | Risk | Effort |
|----|-----------|------------|----------|------|--------|
| P4a | jemalloc | All stages | Unknown throughput delta (memory bounded). May help stage 4's 10.6 GB from allocator churn. | Low | Trivial (feature flag) |
| P4b | `--direct-io` reads | Stages 1, 2, 4 | May hurt (bypasses readahead) or help. BlobReader fadvise already active. | Low | Trivial (CLI flag) |
| P4c | BATCH_SIZE tuning | Stage 4 | Larger = more rayon parallelism. Affects RSS. Currently 32. | Low | Trivial |
| P4d | Pipeline config tuning | Stages 1, 4 | `read_ahead`, `decode_ahead` values. | Low | Trivial |
| P4e | Output compression mode | Stage 4 | `--compression none` for timing vs `zlib` for production. | Low | Trivial |

### Priority 5: Architectural (low payoff)

| ID | Approach | Expected | Risk | Effort |
|----|----------|----------|------|--------|
| ~~P5a~~ | ~~Stage 4 decode_threads(1)~~ | ~~IO overlap without full cross-thread churn.~~ | ~~10-15%~~ | ~~Done~~ | **Measured: 383s (-17%, -78s). RSS 1.8 GB (was 11.8 GB).** |
| P5b | Way-only scanner for stage 1 | Skip string table. Stage 1 is 9% — low payoff. | ~10-20% of 82s | Low | Medium |
| P5c | Parallel stage 3 bucket processing | rayon over 256 buckets. Stage 3 is 8% — low payoff. | ~2-4x of 73s | Low | Low |
| P5d | Planet coord_slots windowed reader | Sequential windowed reader instead of 64 GB mmap at planet scale. | Planet safety only | Low | Medium |

### Recommended test order

1. **P1a** — skip non-node blobs in stage 2. Free 15-20%. Zero risk.
2. **P3a** — precompute range_size divisions. Trivial, ~2-4s.
3. **P2a** — reusable decompress buffer for stage 2. Low effort.
4. **P4a** — jemalloc. Zero code, one run.
5. **P5a** — decode_threads(1) for stage 4. One-line test.
6. **P2b** — parallel tuples for stage 2. Big architectural win.
7. **P1b** — tagdata node blob skipping for stage 4. Medium effort, big win.
8. **P2c** — parallel stage 4.
9. Rest as time permits.

### Theoretical ceiling

If both stages 2 and 4 are parallelized (P1a + P1b):
- Stage 1: 82s
- Stage 2: ~65s (from 331s)
- Stage 3: 73s
- Stage 4: ~120s (from 392s)
- **Total: ~340s (~5.5 min)**

That would be 6.5x faster than the original 2,060s and 7.5x faster than dense (2,565s).

### Done

- [x] Fix stage 4 OOM — sequential reader (commit `2873919`)
- [x] Full end-to-end Europe — 901s (commit `ee9b19f`)
- [x] Node-only scanner for stage 2 — eliminates PrimitiveBlock churn
- [x] Scatter buffer for stage 3 — 15x speedup
- [x] BlobReader fadvise(DONTNEED) — general infrastructure
- [x] Deferred IdSetDense — saves 1.4 GB during stages 1-3
- [x] DecompressPool for stage 4 — buffer reuse
- [x] set_parse_indexdata(false) — stages 2 + 4
- [x] read_exact for bucket loads — exact-size allocation
- [x] Hotpath annotations on all external join functions
- [x] Skip non-node blobs in stage 2 (indexdata check) — commit `d272b49`
- [x] DecompressPool reuse in stage 2 — commit `d272b49`
- [x] Precomputed slot_range_size — **regression** (+24s), reverted
- [x] Tuple intermediary (extract_node_tuples) — **regression** (+11s), reverted for sequential path. Function kept for parallel version.
- [x] jemalloc — no throughput difference on sequential path
- [x] decode_threads(1) for stage 4 — 461s → 383s (-17%), anon RSS 1.6 GB flat (was 11.8 GB)
- [x] decode_threads(2) tested — 320s but 27 GB peak anon. Unsafe for planet. Rejected.
- [x] Periodic RSS logging confirmed decode_threads(1) anon=1574 MB flat for 520K blocks
- [ ] Planet benchmark — 87.7 GB PBF
