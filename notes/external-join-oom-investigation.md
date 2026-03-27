# External join OOM investigation — Europe scale

## Setup
- Europe PBF: 32.4 GB, 4.69B way-node refs, 256 buckets
- Host: 32 GB RAM, ~25 GB free
- External join: 4 stages (way pass, node join, slot reorder, assembly)
- Stage 2 (node join) OOM killed every time at bucket 16/256

## Investigation timeline

### Phase 1: Assumed page cache problem

Initial RSS logging showed stage 2 hitting 21+ GB RSS. We assumed this was
page cache from PBF reads + bucket write dirty pages.

**Attempted fixes (all failed to prevent OOM):**

1. **Periodic flush+fadvise(DONTNEED) on bucket writes** — fadvise only evicts
   clean pages; dirty pages from recent writes survive. No effect on RSS.

2. **sync_data() before fadvise** — works (stage 1 post-finish: 73 MB) but
   256 fdatasync calls × 220 cycles = **4.4x throughput penalty** (108s → 474s).
   Unacceptable.

3. **BlobReader fadvise(DONTNEED) after each blob read** — general infrastructure
   improvement (commit `4ab6976`). Evicts PBF read pages behind read head.
   Stage 1 post-finish dropped to 46 MB. But stage 2 RSS grew identically —
   freeing read pages just let kernel fill that space with more dirty write pages.

4. **sync_file_range(SYNC_FILE_RANGE_WRITE) for async writeback** — non-blocking
   writeback hint. Zero effect on stage 2 RSS growth.

5. **O_DIRECT for bucket writes (DirectWriter)** — bypasses page cache entirely.
   **Zero effect.** Same linear RSS growth.

### Phase 2: RssAnon/RssFile breakdown

Added `/proc/self/status` parsing to distinguish anonymous heap from file-backed
pages. Result:

```
stage2: 28000 blocks, rss=23966MB anon=23962MB file=4MB
```

**file=4MB throughout.** ALL 24+ GB is anonymous heap, not page cache.
Every page cache fix was targeting the wrong problem.

### Phase 3: Allocator theory

Sent findings to reviewers. Consensus: glibc malloc arena retention from
cross-thread alloc/free (IO thread allocates → rayon threads free).

**Tests:**

| Test | Stage 2 RSS growth | Result |
|------|-------------------|--------|
| `MALLOC_ARENA_MAX=2` | Same linear growth | No help |
| `MALLOC_ARENA_MAX=1` | Same growth (delayed start, reuses stage 1 arena) | No help |
| jemalloc (`--features jemalloc`) | Same linear growth | No help |

**Conclusion: NOT allocator arena retention.** Three different allocator configs
produce identical growth. This is a real memory issue in application code.

### Phase 4: Binary search for the leak

**Test 1: Skip all node processing (`continue` in block loop)**
```rust
for block in reader.into_blocks_pipelined() {
    let block = block?;
    continue; // skip everything
}
```
Result: **RSS flat at 383 MB** through 68K blocks. Pipeline is NOT leaking.

**Test 2: Full merge-join but skip slot bucket writes**
```rust
// commented out: writer.write_all(&entry_buf) and entry_counts increment
```
Result: **Same linear growth.** 22+ GB at bucket 16. Writes are not the problem.

**Conclusion:** The leak is NOT in bucket loading (buffer reuse had zero effect).

### Phase 5: Further bisection (element iteration)

**Test: skip everything (`continue` before element loop)**
Result: 383 MB flat through 464K blocks. Pipeline + DecompressPool NOT leaking.

**Test: iterate elements + extract id/lat/lon, skip ALL merge-join logic**
Result: 478 MB → 25168 MB over 464K blocks, then PLATEAUED. Completed.

**Test: full merge-join, disable slot bucket writes only**
Result: same growth. Writes not the problem.

**Test: buffer reuse in load_coo_bucket (clear+refill instead of new Vecs)**
Result: same growth. Bucket loading not the problem.

**Conclusion:** The leak is triggered by element iteration itself. Calling
`block.elements_skip_metadata()` and iterating DenseNode elements causes
~54 KB/block of anonymous heap retention. The plateau proves it's not a
logical leak — the allocator eventually reuses freed memory.

### Phase 6: Allocator and pool diagnostics

**DecompressPool full-drops counter:** 52 drops across 464K blocks (stage 1).
Only 104 MB of pool churn. Pool is NOT the problem.

**MALLOC_ARENA_MAX=1:** Same growth, but stage 1 memory didn't drop at finish()
(single arena can't reclaim as effectively). Stage 2 flat for first 9K blocks
(reusing stage 1's arena space), then grew.

**jemalloc (--features jemalloc):** Same growth. jemalloc's dirty_decay not
returning pages fast enough. Or MADV_FREE pages still count as RssAnon.

## Root cause analysis

Every PrimitiveBlock construction (in rayon decode threads) allocates:
1. `WireStringTable::entries: Box<[(u32, u32)]>` — ~100-1000 entries × 8 bytes
2. `WireBlock::group_ranges: Box<[(u32, u32)]>` — ~1-4 entries × 8 bytes
3. `into_boxed_slice()` reallocs (Vec→Box, freeing Vec overallocation)

These are allocated on rayon decode threads, freed on the consumer thread
when PrimitiveBlock is dropped. Cross-thread alloc/free with varying sizes
causes allocator fragmentation that neither glibc nor jemalloc resolves:
- glibc: holds freed pages in per-thread arenas
- jemalloc: marks MADV_FREE (still counts as RssAnon without pressure)

464K blocks × ~54 KB retained/block = ~25 GB peak before plateau.

The full merge-join OOMs at 27 GB because it adds ~2 GB of live data
(sorted_pairs, writes) on top of the ~25 GB retained free pages.

## Growth pattern

~0.8 GB per 1000 blocks in stage 2. All anonymous heap (RssFile=4MB).
Plateaus at ~25 GB after ~400K blocks (allocator free lists saturate).
16 buckets processed before OOM at 25-27 GB in full merge-join.

## Proposed fixes (to be tested)

**Approach A — Reuse WireBlock allocations:** Thread-local reusable Vecs
for string table entries and group_ranges in the decode path. Eliminates
per-block alloc/free churn.

**Approach B — Node-only scanner:** Bypass PrimitiveBlock entirely for
stage 2. Decompress blob, walk wire format directly for dense node
id/lat/lon. Skips string table parsing, group range collection, UTF-8
validation. Zero per-block heap allocations beyond the decompression buffer.

Key code paths for both approaches:
- `decompress_blob()` in blob.rs (already decoupled from PrimitiveBlock)
- `WireBlock::parse()` in wire.rs (the allocation site)
- `WireDenseNodes::parse()` in wire.rs (zero-alloc, borrows from buffer)
- `DenseNodeIter` in dense.rs (zero-alloc, maintains delta accumulators)
- `PackedSint64Iter` in protohoggr (varint decoder, stack-based)

## What we kept

- **BlobReader fadvise(DONTNEED)** — commit `4ab6976`. General infrastructure
  improvement. Stage 2 RSS 383 MB with pipeline-only drain proves it works.
- **Deferred IdSetDense** — saves 1.4 GB during stages 1-3.
- **sync_data+fadvise in finish()** — effective at stage boundaries.
- **RSS logging with anon/file breakdown** — essential for diagnosis.
- **O_DIRECT bucket writes** — reverted to BufWriter (not the problem).
- **Buffer reuse in load_coo_bucket/load_resolved_bucket** — implemented
  but not the fix. Keep anyway (good practice, helps stage 3).

## Approach B results: Node-only scanner

Replaced `into_blocks_pipelined` + `PrimitiveBlock` in stage 2 with sequential
`BlobReader` + `decompress_raw()` + inline wire format parsing. No string table,
no `WireBlock`, no `Box<[...]>` allocations.

**Denmark results:**
- Stage 2: 142 MB peak RSS, 39 MB post-finish. 6033ms.
- Element counts match dense exactly (3513255 nodes, 6616526 ways, 46103 relations)
- Diff: 0 differences (byte-identical output)

**Europe results (in progress):**
- Stage 2: 1376 MB stable through 522K blocks (4.69B nodes resolved). 353s.
- Post-stage2: 84 MB.
- Stage 3+ in progress.

The node-only scanner eliminates the 25 GB heap retention completely. RSS stays
flat because there are no per-block heap allocations — only the reusable
decompression buffer and the bucket load Vecs.

**Europe full run (stages 1-4):** Stage 4 OOM killed. Same PrimitiveBlock
churn as stage 2 but now with 1380 MB IdSetDense on top. Stage 4 uses the
standard pipelined assembly path (shared with dense/sparse ALTW). Fix
independently via sequential iteration or decode_threads(1).

## Test matrix — stages 1-3 permutations

Stage 4 disabled (early exit after stage 3) while testing optimization
strategies for stages 1-3.

Baseline from the full Europe run above:
- Stage 1: 108s (way pass, BufWriter buckets)
- Stage 2: 315s (node-only scanner, sequential BlobReader)
- Stage 3: 1079s (slot reorder, pwrite per entry)
- Total stages 1-3: ~1502s (~25 min)

### Stage 1 permutations (way pass → bucket writes)

| # | Read path | Write path | Expected effect |
|---|-----------|------------|-----------------|
| A1 | pipelined (current) | BufWriter (current) | baseline |
| A2 | pipelined | DirectWriter (O_DIRECT) | eliminates write cache (was 11 GB) |

### Stage 2 permutations (node join)

| # | Decode path | Notes |
|---|-------------|-------|
| B1 | node-only scanner (current) | sequential, no PrimitiveBlock |
| B2 | pipelined + PrimitiveBlock | original path, for comparison (will OOM without fix) |

### Stage 3 permutations (slot reorder)

Stage 3 is the new bottleneck at 1079s. It does 256 sequential bucket loads,
sorts each, then pwrite per entry to coord_slots.

| # | Strategy | Expected effect |
|---|----------|-----------------|
| C1 | current (pwrite per entry) | baseline 1079s |
| C2 | buffered writer (BufWriter wrapping pwrite) | reduce syscall count |
| C3 | mmap coord_slots + memcpy | eliminate pwrite entirely |
| C4 | parallel bucket processing (rayon) | utilize multiple cores |

### Cross-cutting permutations

| # | Feature | Applies to |
|---|---------|------------|
| D1 | BlobReader fadvise (current) | stages 1, 2 reads |
| D2 | --direct-io on PBF reads | stages 1, 2 reads |
| D3 | jemalloc | all stages |
| D4 | sync_data+fadvise in finish() (current) | stage 1, 2 boundary |

### Stage 3 deep dive — why 1079s

4.69B entries × 8-byte `write_at` each = 4.69B `pwrite64` syscalls.
At ~200ns/syscall on NVMe = ~938s of pure syscall overhead. Remaining ~141s
is bucket I/O + sort. The pwrite storm is the bottleneck.

Key insight (from perf reviewers): the slot buckets already partition the
slot_pos space in ascending order. Processing buckets 0→255 produces
globally sequential output. The current code treats this as random I/O
when it's actually sequential.

#### Stage 3 approach 1: Sequential BufWriter with sentinel fill

Process buckets 0→255. For each bucket, sort entries by slot_pos (already done).
Write entries sequentially to a BufWriter, filling gaps with zero sentinels:

```
for bucket 0..256:
    load + sort entries
    for each entry:
        write zero sentinels for gap between last_slot and entry.slot_pos
        write (lat, lon) as 8 bytes
write trailing sentinels to total_slots
```

Replaces 4.69B pwrite64 with ~144K write calls (37 GB / 256 KB BufWriter).
Sequential write at ~3 GB/s ≈ 12s. Plus bucket I/O + sort ≈ 141s.
**Expected: ~150-160s** (7x speedup).

Pro: simple, no memory risk, sequential I/O.
Con: writes 37 GB including gaps (sentinels), even for sparse distributions.

#### Stage 3 approach 2: Dense scatter buffer per bucket

For each bucket, compute its slot range from the partitioning. Allocate a
zeroed buffer covering that range (bucket_slot_count × 8 bytes). Scatter
entries directly by position — no sort needed:

```
for bucket 0..256:
    let range_slots = total_slots / 256  (approx)
    let mut buf = vec![0u8; range_slots * 8]
    load bucket entries
    for each entry:
        buf[(entry.slot_pos - bucket_start) * 8 ..] = (lat, lon)
    write_all(buf)
```

Eliminates: 4.69B pwrite syscalls, bucket sort entirely, random writes.
One write_all per bucket = 256 write calls total.
**Expected: comparable to approach 1, possibly faster (no sort).**

Pro: eliminates sort, simplest write pattern (one write_all per bucket).
Con: allocates a buffer sized to the bucket's full slot range (~146 MB
at Europe scale: 37.5 GB / 256). Reusable across buckets (high-water).

#### Stage 3 approach 3: mmap coord_slots + memcpy

mmap the pre-allocated coord_slots file as MmapMut. Write entries via
memory copy instead of pwrite. Zero syscalls during write loop.

Pro: zero syscalls, no gap fill, works with any access pattern.
Con: 37 GB mmap on 32 GB host may cause memory pressure. Dirty pages
from mmap writes have the same kernel writeback issues we saw in stage 1.

Lower priority — approaches 1-2 avoid mmap complexity.

### Priority order

1. **C (stage 3)** — 72% of total time. Test approaches 1 and 2.
2. **A2** — measure O_DIRECT bucket write throughput impact on stage 1
3. **B (stage 2)** — profile node-only scanner for further optimization
4. **D2** — --direct-io vs buffered read comparison with fadvise
5. **D3** — jemalloc vs glibc throughput comparison

### Transferable insights

These patterns apply beyond external join:

- **Sequential BufWriter with gap fill** — any command writing a sparse
  positional file (geocode index cells, renumber output).
- **Dense scatter buffer** — any radix-bucketed workflow where the bucket
  partitioning defines the output order. Eliminates sort when position
  is computable from the entry.
- **Node-only scanner** — any command that only needs id/lat/lon from node
  blobs. Bypassing PrimitiveBlock avoids string table + group range
  allocations that cause heap retention at scale.
- **BlobReader fadvise(DONTNEED)** — general infrastructure, benefits all
  single-pass forward scans.
- **RssAnon/RssFile breakdown** — essential for diagnosing memory issues.
  Without this, we chased page cache for hours when the problem was heap.
