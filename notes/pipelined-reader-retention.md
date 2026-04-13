# Pipelined reader cross-thread retention audit

## The problem (historical — largely resolved)

`into_blocks_pipelined` (in `src/read/pipeline.rs`) spawns rayon tasks
that decompress and parse blobs into `PrimitiveBlock` objects. These are
sent via channel to the consumer thread. The `PrimitiveBlock` owns its
decompressed buffer (`Bytes`), which was allocated on the rayon worker
thread. When the consumer drops the `PrimitiveBlock`, the underlying
`Vec<u8>` is freed on the consumer thread — a cross-thread free.

Neither glibc nor jemalloc efficiently returns cross-thread-freed memory
to the OS. At planet scale (600K+ blobs × 50-200 KB each), the consumer
accumulates 10-25+ GB of cross-thread-freed memory that appears as RSS
growth even though the buffers are logically freed.

**Status (April 2026):** The `DecompressPool` + `from_vec_pooled_with_scratch`
pattern was added to the pipelined reader itself in commit `8f6999b`
(2026-03-30). Workers call `blob.to_primitiveblock_inline_with_scratch(&bp,
st, gr)` which uses `pool_get` / `pool_wrap` — decompression buffers are
recycled via `PooledBuffer::drop` instead of being cross-thread freed.
The pool holds up to 64 buffers (MAX_POOL_SIZE) capped at 4 MB each
(MAX_RETAINED_CAPACITY). `parse_and_inline` scratch Vecs are thread-local
(same commit). `PrimitiveBlock` is zero-copy beyond the buffer —
`WireBlock<'static>` borrows from `buffer: Bytes`, no separate heap
allocations.

**The original 10-25 GB retention problem is solved for all pipelined
reader paths.** The ~64 buffer objects circulate through the pool for the
lifetime of the file; there is no cumulative cross-thread free churn.

## Remaining architectural concern: thread oversubscription

The pipelined reader creates its own rayon `ThreadPool` with
`available_parallelism() - 2` threads (`pipeline.rs:157`). Commands that
do parallel batch processing via `par_iter()` use the global rayon pool.
Both pools run concurrently — decode never pauses while the consumer
processes a batch.

On an 8-core machine: 6 decode threads + 8 global pool threads = 14
threads contending for 8 cores. The decode pool produces blocks into a
channel while the consumer collects up to BATCH_SIZE=64 blocks before
dispatching to the global pool for parallel processing. During batch
processing, the decode pool continues producing — filling the channel
for the next batch.

This is not a proven bottleneck, but it means every batch-processing
command pays for concurrent decode + process thread pools whether or not
that concurrency helps. Commands where per-block processing is heavier
than decode benefit from the overlap. Commands where per-block work is
lightweight may lose more to oversubscription than they gain from
decode overlap.

## Current state (April 2026)

Commands that still use `into_blocks_pipelined`:

### 1. renumber

**Path:** `reader.into_blocks_pipelined()` at line 79
**Impact:** Every element at planet scale. 11.6B elements, 600K blocks.
Renumber is sequential (can't parallelize — ID assignment is order-dependent),
so pipelined decode only helps overlap decompression with encoding.
**Status:** Being converted to external join architecture in current work
(separate from this audit). See `src/commands/renumber_external.rs`.

### 2. getid (pass 2 write phase)

**Path:** `reader.into_blocks_pipelined()` at line 452
**Impact:** Pass 2 write phase for `--add-referenced` only. The single-pass
path (`getid` without `--add-referenced` and `removeid`) uses `filter_by_id`
which reads raw frames directly — no pipelined decode at all. Classification
passes already use `parallel_classify_phase` (converted).
**Retention:** Solved — DecompressPool recycles buffers. Batch bounding
(BATCH_SIZE=64) limits live blocks. No retention concern.
**Performance (investigated April 2026):** Per-block work is lightweight,
same profile as getparents. The ID set is small (requested IDs + their
referenced nodes). Most blobs are already skipped via `BlobFilter` (type
filtering on the requested ID types). Within non-skipped blobs, per-element
work is an `IdSetDense::get` check (O(1) bitset lookup) — most elements
are skipped. Only matching elements go through BlockBuilder re-encode.
The `par_iter` batch processing dispatches 64 blocks to rayon workers
where each worker does mostly skip-skip-skip with occasional writes.
Same analysis as getparents: rayon scheduling overhead likely exceeds
the actual per-block work. Candidate for sequential conversion.
**Priority:** Low — niche path (`--add-referenced` only), lightweight
per-block work. Convert to sequential BlobReader + par_iter or fully
sequential if measurement shows benefit.

### 3. tags_filter (single-pass `-R` path)

**Path:** `reader.into_blocks_pipelined()` at line 313
**Impact:** The single-pass path (no `--add-referenced-ways`). Processes
elements via `for_each_primitive_block_batch`.
**Retention:** Solved — DecompressPool recycles buffers.
**Performance (investigated April 2026):** Per-block work is substantially
heavier than getparents — every element's tags are collected into a buffer
and matched against N expressions via `element_matches`, then matching
elements go through full BlockBuilder re-encode (string table construction,
varint encoding, delta packing). Most elements are touched, not skipped.
The `par_iter` batch processing is genuinely valuable for this workload.

The open question is whether the **pipelined decode** (concurrent with
batch processing) helps or hurts on top of the parallel batch. Two rayon
pools run simultaneously: the decode pool (N-2 threads) produces blocks
into a channel while the global pool processes the previous batch. On an
8-core host this means ~14 threads on 8 cores. Converting to sequential
decode + par_iter batches would eliminate the oversubscription but lose
the decode/processing overlap.

The two-pass path (the planet-scale production path with
`--add-referenced-ways`) already uses pread workers and doesn't go
through this code. This single-pass path is for simple tag filters
without dependency expansion.
**Priority:** Low — par_iter is justified, pipelined decode benefit is
unproven but plausible for this workload. Convert to sequential decode
+ par_iter if measurement shows oversubscription hurts.

### 4. add_locations_to_ways (decode-all fallback)

**Path:** `reader.into_blocks_pipelined()` at line 1066
**Impact:** Only triggered with `--force` on non-indexed PBFs.
**Retention:** Solved — DecompressPool recycles buffers even though
this path uses manual batching (same pool, same recycling). The pool's
64-buffer cap bounds live allocations regardless of batch size.
**Performance (investigated April 2026):** Heaviest per-block work of
any pipelined path. Every element is processed — no skip path:
- Nodes: tag check + conditional BlockBuilder write (most nodes written)
- Ways: collect all refs, look up every node location from index
  (random access into dense mmap or pre-resolved map), then
  `add_way_with_locations` (larger than regular `add_way`)
- Relations: full member collection + BlockBuilder write
- For sparse index: `resolve_batch_locations` pre-resolves all way
  node coordinates via sorted sequential scan before par_iter

The par_iter is doing real work here — location lookups + BlockBuilder
re-encode for every element in the batch. The pipelined decode overlap
is also most valuable here because decode time is a smaller fraction of
total per-block processing time (processing dominates).
**Priority:** Low — niche `--force` path. Both par_iter and pipelined
decode are justified. No conversion recommended.

### 5. cat (type-filtered path)

**Path:** `reader.into_blocks_pipelined()` at line 369
**Impact:** `cat --type` for non-passthrough blobs. Uses
`for_each_primitive_block_batch_budgeted` with BATCH_SIZE=64 and 32 MB
byte budget, so retention was already bounded pre-pool. With the pool,
no retention concern at all.
**Performance (investigated April 2026):** Heaviest batch work in the
codebase. Each rayon worker does type filtering + BlockBuilder re-encode
AND zlib compression + blob framing (`frame_blob_pipelined`) — all
inside the par_iter. Compression is genuinely CPU-heavy, making the
par_iter essential. The pipelined decode overlap is also most valuable
here because batch processing (with compression) takes longer than
decode — the decode pool produces the next batch while the current
batch is still compressing.
**Priority:** Low — no conversion recommended. Both par_iter and
pipelined decode are justified and working well. Best-optimized of all
pipelined paths.

### 6. getparents

**Path:** `reader.into_blocks_pipelined()` at line 68
**Impact:** Reads the entire PBF to find parent elements. Single-pass
with `for_each_primitive_block_batch`.
**Retention:** Solved — DecompressPool recycles buffers.
**Performance (investigated April 2026):** Sequential conversion was
attempted (commit `c912e4d`) and **reverted** — 4.7x regression on
Denmark (1400ms vs 300ms baseline). The hypothesis that rayon par_iter
overhead exceeded per-block work was wrong. Decompression is the
dominant cost, not the ID lookups. The pipelined reader's parallel
decode provides real throughput even when per-block processing is
lightweight. This finding also rules out sequential conversion for
getid pass 2 (same profile).
**Priority:** No action — pipelined decode is justified even for
lightweight per-block work.

## Commands already converted (no pipelined retention)

- **node_stats** — sequential BlobReader + reusable decompress_buf
- **tags_count** — sequential BlobReader + reusable decompress_buf
- **check_refs** — sequential BlobReader with new_with_scratch
- **tags_filter two-pass** — pread workers (parallel_classify_phase)
- **extract (all strategies)** — pread workers
- **sort** — sequential seek-based reads (pread per blob)
- **merge** — pread-based passthrough + sweep merge
- **diff/derive_changes** — StreamingBlocks (sequential + DecompressPool)
- **external_join** — pread workers + sequential BlobReader

## Recommendation

The original cross-thread buffer retention problem is **solved** for all
pipelined reader paths. The `DecompressPool` (commit `8f6999b`) recycles
decompression buffers via `PooledBuffer::drop` — no cumulative cross-thread
free churn, no RSS growth from freed-but-retained memory.

**Thread oversubscription** (two concurrent rayon pools: decode + batch
processing) is the remaining architectural concern. Per-command
investigation (April 2026) found that **sequential conversion is not
beneficial** — even for lightweight per-block work (getparents), the
pipelined reader's parallel decode provides real throughput because
decompression dominates, not processing.

getparents sequential conversion was attempted (commit `c912e4d`) and
reverted: 4.7x regression on Denmark (1400ms vs 300ms). This rules out
sequential conversion for all remaining pipelined paths — if it doesn't
help for the lightest workload, it won't help for heavier ones.

**No conversions recommended.** All remaining pipelined paths (getid,
tags_filter, cat --type, ALTW decode-all, getparents) are correctly
using pipelined decode + par_iter batch processing. The thread
oversubscription concern is real but the decode parallelism benefit
outweighs it at every measured scale.

**renumber** is being converted to an external join architecture in
current work — not driven by retention or oversubscription concerns
but by the need for planet-scale memory-bounded ID remapping.
