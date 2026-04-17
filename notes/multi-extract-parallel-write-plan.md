# Multi-extract parallel write phases: implementation plan

Prerequisite: [multi-extract-optimization.md](multi-extract-optimization.md)
for the full analysis.

## Problem

Multi-extract single-pass (`try_extract_multi_single_pass`) has three
write phases (nodes, ways, relations) that use sequential BlobReader
with single-threaded decode. Classification is already parallel
(`parallel_classify_phase`), but the write phases are the bottleneck.

Japan timing: 34.9s single-pass vs 5 × 4.4s = 22s sequential (each
sequential extract gets parallel decode via `pread_execute`).

## Proposed architecture

Convert each write phase from:
```
sequential BlobReader → single-threaded decode → N-way write
```
to:
```
pread-from-workers → parallel decode → consumer routes to N writers
```

Reuse the `pread_execute` infrastructure from single-extract, adapted
for N-region routing.

## Key difference from single-extract

Single-extract `pread_execute` has one writer. Each worker produces
`Vec<OwnedBlock>` for one region. The consumer writes them sequentially.

Multi-extract needs N writers. Each worker must classify each element
against N regions and produce N × `Vec<OwnedBlock>`. The consumer
routes blocks to N writers.

### Worker output type

```rust
struct MultiExtractWorkerResult {
    /// Per-region owned blocks. region_blocks[i] is the blocks for region i.
    region_blocks: Vec<Vec<OwnedBlock>>,
    /// Per-region stats.
    region_stats: Vec<ExtractStats>,
}
```

Workers classify each element against all N regions and build N
BlockBuilders. When a BlockBuilder fills (8000 elements), it produces
an OwnedBlock for that region.

### Memory concern

Each worker holds N BlockBuilders simultaneously. At N=10, that's
10 × ~500 KB = 5 MB per worker. With 8 workers, 40 MB. Acceptable.

At N=100, it's 50 MB per worker, 400 MB total. Still acceptable but
approaching the limit. The spatial index (TODO.md) would reduce the
per-element classification cost for large N, but doesn't reduce the
N-BlockBuilder memory.

### Consumer routing

The consumer receives `MultiExtractWorkerResult` from the reorder buffer
and writes each region's blocks to the corresponding writer:

```rust
for (i, blocks) in result.region_blocks.iter().enumerate() {
    for (bytes, index, tagdata) in blocks {
        writers[i].write_primitive_block_owned(bytes, index, tagdata.as_deref())?;
    }
}
```

## Status - DONE (commit `9f72bcf`)

Steps 1-3 shipped. `multi_extract_pread_write` in `src/commands/extract.rs`
replaces all three sequential write phases. Denmark 5-region: 6.7s → 2.1s
(3.2x). Japan 5-region: 32.5s → 8.1s (4.0x). Verified via
`brokkr verify multi-extract --regions 3` (PASS) and `--regions 5`
(strip-4 known rounding issue, pre-existing).

Step 4 (raw passthrough) and step 5 (pipelined writers) remain as
future optimizations.

## Implementation steps

### Step 1: Extract multi-extract write loop into a function - DONE

Currently, each write phase (nodes/ways/relations) has an inline
BlobReader loop. Extract into a shared function:

```rust
fn multi_extract_write_phase(
    input: &Path,
    n: usize,
    kind_filter: ElemKind,
    id_sets: &[&IdSetDense],  // per-region ID sets for this element type
    bbs: &mut [BlockBuilder],  // per-region BlockBuilders
    writers: &mut [PbfWriter<BufWriter<File>>],
    stats: &mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
    direct_io: bool,
    spatial_filter: &BlobFilter,
) -> Result<()>;
```

### Step 2: Build schedule from indexdata - DONE

The schedule is the same as what `build_classify_schedule` produces.
Multi-extract already builds node/way/relation schedules for the
classification phases. Reuse those schedules for the write phases
(they have the same blob offsets).

Actually, looking at the current code, the classification schedules
are `Vec<(usize, u64, usize)>` tuples, but the write phases need
to iterate all blobs of a given type (not just the ones in the
schedule). Wait - the write phases DO filter by type:

```rust
if let Some(idx) = blob.index() {
    if !matches!(idx.kind, ElemKind::Node) { continue; }
    if !spatial_filter.wants_index(&idx) { continue; }
}
```

So the schedule for the write phase is the same as the classification
schedule. We can reuse the `node_schedule`, `way_schedule`,
`relation_schedule` vectors.

### Step 3: Convert to pread-from-workers - DONE

Replace the sequential BlobReader loop with the pread pattern:

```rust
fn multi_extract_pread_write<F>(
    shared_file: &Arc<File>,
    schedule: &[(usize, u64, usize)],
    n: usize,
    classify_fn: F,
    writers: &mut [PbfWriter<BufWriter<File>>],
    stats: &mut [ExtractStats],
) -> Result<()>
where
    F: Fn(&PrimitiveBlock) -> Vec<Vec<OwnedBlock>> + Sync,
{
    // Dispatcher: feed schedule items to workers via channel
    // Workers: pread → decompress → PrimitiveBlock → classify against N regions
    //          → produce N × Vec<OwnedBlock>
    // Consumer: reorder by sequence, write each region's blocks to its writer
}
```

The per-worker closure builds N BlockBuilders, iterates elements,
and classifies each against N regions. When a BlockBuilder fills,
it adds an OwnedBlock to the corresponding region's output Vec.

### Step 4: Raw passthrough for contained node blobs

For node blobs fully contained in a region's bbox (indexdata bbox
⊆ region bbox), write the raw compressed frame directly to that
region's writer. No decompression, no re-encoding.

This is the same optimization as single-extract's `raw_passthrough`
flag, extended to per-region decisions:

```rust
for (i, bbox) in bbox_ints.iter().enumerate() {
    if blob_bbox.is_contained_in(bbox) {
        // Raw passthrough: pread the frame and write directly
        writers[i].write_raw_owned(frame_bytes)?;
        stats[i].nodes_in_bbox += blob_count;
    }
}
```

A blob can be passthrough for multiple regions simultaneously (if
it's contained in all of them).

Blobs that are passthrough for ALL overlapping regions skip decode
entirely. Blobs that are passthrough for some but not all must be
decoded for the non-passthrough regions but can be written raw to
the passthrough regions.

### Step 5: Sync vs pipelined writers

The current implementation uses sync-mode PbfWriters (one per region).
For N regions, this means compression is sequential within each region.

Options:
- **Sync writers (current):** Simple. Compression happens in the
  consumer thread when writing each OwnedBlock. One compression per
  block, sequential across regions within a batch.
- **Pipelined writers:** Each writer has its own compression pipeline
  (rayon pool + writer thread). N × rayon contention. Higher throughput
  for large N but more complex.

Recommendation: **keep sync writers for v1.** The parallel decode is
the main win. Compression parallelism can be added later if benchmarks
show it's the bottleneck.

## Testing

- `brokkr verify multi-extract --regions 3` (and 4, 5)
- Compare element counts between sequential and parallel implementations
- Verify output file hashes match (if using deterministic compression)

## Relationship to other work

- Pattern from `pread_execute` in single-extract - needs a new
  function but the dispatcher/worker/consumer architecture is identical
- Blocked by: nothing (can start immediately)
- Enables: raw passthrough optimization (step 4)
- Complementary: spatial index for large N (separate TODO item)
- Does NOT require columnar decode (that's for the classification
  phase, which is already parallel)

## Review feedback (April 2026, Opus reviewer)

- **`pread_execute` reuse:** NEEDS_REDESIGN - can't reuse directly
  (hardcoded for single region), but the pattern is well-established.
  Write a new `multi_extract_pread_write` function with the same
  dispatcher/worker/consumer shape.
- **BlockBuilder not Send:** Non-issue. Created thread-local inside
  worker closures, same as existing `pread_execute`. Produces
  `Vec<OwnedBlock>` which is `Send`.
- **Output size:** FEASIBLE. Most elements match 0-2 regions (bboxes
  rarely overlap heavily). Memory per worker batch is bounded.
- **Reorder buffer:** FEASIBLE. `ReorderBuffer<T>` is generic. Use
  `T = MultiExtractWorkerResult` containing `Vec<Vec<OwnedBlock>>`.
  Per-region ordering guaranteed by input sequence preservation.
- **BlobBbox at schedule time:** FEASIBLE. `BlobDesc.bbox` already
  populated from indexdata.
- **Memory at N=50:** ~200 MB builder overhead across 8 workers.
  Acceptable. N=100+ approaches 400 MB - worth monitoring.
