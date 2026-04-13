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

### 2. getid (one-pass batch path)

**Path:** `reader.into_blocks_pipelined()` at line 452
**Impact:** Pass 2 write phase for `--add-referenced`. Classification
passes already use `parallel_classify_phase` (converted). The batch path
processes one batch of blocks at a time via `for_each_primitive_block_batch`.
**Retention:** Solved — DecompressPool recycles buffers. Batch bounding
(BATCH_SIZE=64) limits live blocks. No retention concern.
**Performance:** Parallel batch processing is valuable — per-block work
includes ID filtering + BlockBuilder re-encode. The decode/process
overlap from pipelining helps here.
**Priority:** Low — no action needed.

### 3. tags_filter (single-pass `-R` path)

**Path:** `reader.into_blocks_pipelined()` at line 313
**Impact:** The single-pass path (no `--add-referenced-ways`). Processes
elements via `for_each_primitive_block_batch`.
**Retention:** Solved — DecompressPool recycles buffers.
**Performance:** Parallel batch processing is valuable — tag expression
matching + BlockBuilder re-encode is meaningful per-block CPU work. The
two-pass path (the planet-scale production path) already uses pread
workers; this single-pass path is for simple tag filters.
**Priority:** Low — no action needed.

### 4. add_locations_to_ways (decode-all fallback)

**Path:** `reader.into_blocks_pipelined()` at line 1066
**Impact:** Only triggered with `--force` on non-indexed PBFs.
**Retention:** Solved — DecompressPool recycles buffers even though
this path uses manual batching (same pool, same recycling). The pool's
64-buffer cap bounds live allocations regardless of batch size.
**Performance:** Parallel batch processing is valuable — location
enrichment + BlockBuilder re-encode is the heaviest per-block work of
any command here.
**Priority:** Low — niche path, retention solved, parallel processing
valuable.

### 5. cat (type-filtered path)

**Path:** `reader.into_blocks_pipelined()` at line 369
**Impact:** `cat --type` for non-passthrough blobs. Uses
`for_each_primitive_block_batch_budgeted` with BATCH_SIZE=64 and 32 MB
byte budget, so retention was already bounded pre-pool. With the pool,
no retention concern at all.
**Performance:** Well-tuned. Dual-bounded batches prevent memory spikes
on large blocks. Parallel batch processing (`process_batch`) does
parallel encode+compress — the heaviest batch work in the codebase.
**Priority:** Low — no action needed. Best-optimized of all pipelined
paths.

### 6. getparents

**Path:** `reader.into_blocks_pipelined()` at line 68
**Impact:** Reads the entire PBF to find parent elements. Single-pass
with `for_each_primitive_block_batch`.
**Retention:** Solved — DecompressPool recycles buffers.
**Performance:** `process_batch` uses `par_iter`, but per-block work is
lightweight — just ID lookups (`IdSetDense::get`) and conditional writes
to BlockBuilder. Most elements are skipped (only parents of a small ID
set are emitted). The rayon `par_iter` overhead (task scheduling, work
stealing) may exceed the actual per-block work. This is the one command
where sequential processing might outperform the current parallel batch
pattern.
**Priority:** Low — niche diagnostic command. If optimizing, try
sequential BlobReader first to eliminate both the decode pool overhead
and the par_iter overhead. Measure before converting.

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

No urgent conversions are needed. The remaining paths are either
well-optimized (cat --type), benefit from parallel batch processing
(tags_filter, getid, ALTW), or are niche (getparents).

**Thread oversubscription** (two concurrent rayon pools: decode + batch
processing) is the remaining architectural concern. It is not a proven
bottleneck — the decode pool provides I/O-decode overlap that benefits
commands with heavy per-block processing. Measure before converting any
path to sequential decode.

**getparents** is the only command where the pipelined + parallel batch
pattern may be net-negative: per-block work is so lightweight that rayon
overhead likely dominates. A sequential BlobReader with inline processing
would be simpler and potentially faster. Measure on a real workload before
converting.

**renumber** is being converted to an external join architecture in
current work — not driven by retention concerns (which are solved) but
by the need for planet-scale memory-bounded ID remapping.
