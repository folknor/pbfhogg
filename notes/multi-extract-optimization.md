# Multi-extract optimization research

## Current architecture

Single-pass multi-extract (`try_extract_multi_single_pass` in
`src/commands/extract.rs`) reads the PBF once and writes N output files.
Three-phase barrier: nodes → ways → relations.

Each phase has two sub-steps:
1. **Classification** — `parallel_classify_phase` with pread workers
   (parallel decode, per-region ID collection)
2. **Write** — sequential BlobReader (single-threaded decode), write
   matching elements to N sync-mode PbfWriters

## Performance profile

Classification is fast (parallel pread workers). The write phase is
the bottleneck — sequential single-threaded decode for every element.

Japan timing (commit `542aad0`): 34.9s single-pass vs 5 × 4.4s = 22s
sequential. Single-pass is slower at small scale because sequential
extract gets parallel decode per-region, while multi-extract uses
single-threaded decode in the write phase.

At planet scale with 10+ regions, I/O savings (1× vs 10× file reads)
should dominate. But the single-threaded write phase limits throughput.

## Identified issues (code audit)

### 1. Missing scratch buffer reuse in write phases

Lines 788, 867, 936: `PrimitiveBlock::new(decompressed)` instead of
`new_with_scratch(decompressed, &mut st_scratch, &mut gr_scratch)`.

The classification phases use `parallel_classify_phase` which handles
scratch internally. The write phases use manual BlobReader loops that
don't reuse scratch buffers. Fix: add `st_scratch`/`gr_scratch` Vecs
before the write loop and use `new_with_scratch`.

**Impact:** Mechanical fix. Eliminates ~829 MB alloc churn per phase
at Japan scale (same pattern as the `parse_and_inline` scratch win).

### 2. Per-block Vec<Vec<i64>> allocation in classify closures

Lines 738, 832, 897: `vec![Vec::new(); n]` allocates N empty Vecs per
worker per block. For N=10 regions and 30K blocks with 8 workers, that's
~240K Vec allocations (small but unnecessary).

Fix: use `thread_local!` storage for the `region_ids` Vec<Vec<i64>>,
clearing inner Vecs between blocks (same pattern as `COLUMNS` in the
single-extract columnar path).

**Impact:** Minor — Vec<Vec<i64>> is small. But eliminates allocator
churn in the hot classification loop.

### 3. Node classification doesn't use columnar decode

The single-extract path uses `DenseNodeColumns` for bbox classification
(line 2216), but multi-extract uses element-by-element iteration
(line 739). With columnar decode, the classification loop operates on
contiguous i32 arrays — better cache utilization and potential
autovectorization for the N-region inner loop.

For multi-extract with bbox regions, columnar decode is even more
valuable: the inner loop tests each node against N regions. With
columnar layout, this becomes N passes over the same contiguous
lat/lon arrays, or a single pass with N bbox tests per element.

**Impact:** Moderate. Depends on N (number of regions) and whether
the classify loop is a significant fraction of total time. At planet
scale with 10+ regions, this could be substantial.

### 4. Sequential write phases (the main bottleneck)

The write phases use sequential `BlobReader` with single-threaded
decode. Converting to pread-from-workers is complex because the
write side needs to maintain N BlockBuilders and N PbfWriters, and
the ordering must be preserved (nodes written in ascending ID order
per region).

Architecture options:

**Option A: Parallel decode, sequential write**
Workers pread + decompress + PrimitiveBlock, send to consumer.
Consumer iterates elements, routes to N BlockBuilders/writers.
Similar to `pread_execute` in single-extract.

Pros: straightforward, reuses existing pread infrastructure.
Cons: consumer is still single-threaded (BlockBuilder encoding +
N-way routing). The decode is parallel but the encode is serial.

**Option B: Per-region parallel writers**
Each region gets a pipelined PbfWriter (rayon compression).
Consumer routes elements to N pipelined writers. Compression
happens in parallel across regions.

Pros: compression parallelism scales with N regions.
Cons: N × rayon pool contention. Sync-mode writers were chosen
to avoid this. Memory: N × WRITE_AHEAD × blob_size in-flight.

**Option C: Batch-parallel encode**
Decode blocks into a batch, parallel-encode N regions per batch.
Each rayon task encodes one region's matching elements for one block.

Pros: fine-grained parallelism. No ordering issues within a batch.
Cons: complex. Elements from one block may not fill a BlockBuilder
(8000 element limit), so cross-block state is needed.

**Recommendation: Option A** for simplicity. The decode is the main
cost (~70% of write phase time). Parallel decode alone would recover
most of the throughput gap vs sequential per-region extract.

### 5. Spatial index for large N

Currently O(N) per element for region classification (linear scan
through N bboxes). For N > 50, a grid index would be better:
3600×1800 cells of 0.1°, precompute overlapping regions per cell.
Per-element lookup becomes O(1) grid probe + check overlapping
regions in that cell.

**Impact:** Only matters for large N (50+ regions). For typical
multi-extract (5-20 regions), linear scan is fine.

### 6. Raw passthrough for fully-contained node blobs

If a node blob's bbox is fully contained within region R's bbox,
all nodes in that blob match region R. The blob can be written as
a raw compressed frame to region R's writer, skipping decode +
re-encode entirely.

This is the same optimization as single-extract's raw passthrough
(`raw_passthrough` flag on `BlobDesc`), extended to per-region
decisions: for each blob, for each region, check containment.

**Impact:** High at planet scale. 90%+ of node blobs are interior
to at least one region in a typical multi-extract configuration.

## Priority order

1. ~~**Scratch buffer reuse**~~ — DONE (commit `19f8bc9`). `new_with_scratch`
   in all 3 write phases.
2. **Raw passthrough for contained blobs** (high impact, moderate effort)
3. ~~**Parallel decode in write phase**~~ — DONE (commit `9f72bcf`).
   `multi_extract_pread_write` replaces sequential BlobReader in all 3
   write phases. Denmark 5-region: 6.7s → 2.1s (3.2x). Japan 5-region:
   32.5s → 8.1s (4.0x).
4. **Columnar node classification** (moderate impact, low effort)
5. **Thread-local region_ids** (low impact, mechanical)
6. **Spatial index** (only for N > 50, future)

## Relationship to other TODO items

- Scratch buffer reuse connects to the arena allocator research
  (notes/arena-allocator-research.md, step 1)
- Columnar node classification connects to columnar decode stabilization
  (TODO.md Milestone A, item 2)
- Raw passthrough connects to the per-group raw passthrough scaffolding
  (notes/raw-group-passthrough.md) but operates at blob level
