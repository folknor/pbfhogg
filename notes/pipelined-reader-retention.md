# Pipelined reader cross-thread retention audit

## The problem

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

The `DecompressPool` + `from_vec_pooled_with_scratch` pattern solves this:
buffers are returned to the pool (same-thread via Sender) instead of freed,
and recycled by workers on the next blob. This eliminates cross-thread
retention entirely.

## Current state (April 2026)

Commands that still use `into_blocks_pipelined`:

### 1. renumber

**Path:** `reader.into_blocks_pipelined()` at line 79
**Impact:** Every element at planet scale. 11.6B elements, 600K blocks.
Renumber is sequential (can't parallelize — ID assignment is order-dependent),
so pipelined decode only helps overlap decompression with encoding.
**Fix:** Convert to sequential BlobReader + DecompressPool, same as
node_stats/tags_count pattern. Slight decode throughput loss offset by
zero cross-thread retention.
**Priority:** Medium — renumber is rarely used on planet-scale files.

### 2. getid (one-pass batch path)

**Path:** `reader.into_blocks_pipelined()` at line 556
**Impact:** The one-pass batch path for `--add-referenced` when used with
`for_each_primitive_block_batch`. Classification passes already use
`parallel_classify_phase` (converted). The batch path processes one batch
of blocks at a time, so retention is bounded by BATCH_SIZE × block_size.
**Priority:** Low — batch size bounds retention, and the classification
hot path is already converted.

### 3. tags_filter (single-pass `-R` path)

**Path:** `reader.into_blocks_pipelined()` at line 313
**Impact:** The single-pass path (no `--add-referenced-ways`). Processes
elements via `for_each_primitive_block_batch`. Retention bounded by
BATCH_SIZE.
**Priority:** Low — the two-pass path (the planet-scale production path)
already uses pread workers. The single-pass path is only for simple
tag filters without dependency expansion.

### 4. add_locations_to_ways (decode-all fallback)

**Path:** `reader.into_blocks_pipelined()` at line 1064
**Impact:** Only triggered with `--force` on non-indexed PBFs. At planet
scale, this path OOMs due to 25+ GB retention (documented in TODO.md).
**Fix:** Convert to sequential BlobReader + DecompressPool, or better:
refuse `--force` at planet scale and require indexed input.
**Priority:** Low — niche path, already documented as "last unmitigated
retention path."

### 5. cat (type-filtered path)

**Path:** `reader.into_blocks_pipelined()` at line 371
**Impact:** `cat --type` for non-passthrough blobs. Uses
`for_each_primitive_block_batch_budgeted` with BATCH_SIZE=64 and 32 MB
budget, so retention is bounded to ~3.2 MB of live PrimitiveBlocks.
Cross-thread free churn is ~21 GB cumulative at planet scale (430K node
blobs × 50 KB) but RSS is bounded by the batch. The batch also enables
parallel rayon processing of blocks (`process_batch`).
**Fix:** Not recommended — batch pattern already bounds retention, and
converting to sequential would lose the parallel batch processing that
`process_batch` uses for parallel encode+compress.
**Priority:** Low — retention is bounded, parallel processing is valuable.

### 6. getparents

**Path:** `reader.into_blocks_pipelined()` at line 65
**Impact:** Reads the entire PBF to find parent elements. Single-pass
with `for_each_primitive_block_batch`. Retention bounded by BATCH_SIZE.
**Priority:** Low — niche diagnostic command, batch-bounded retention.

## Commands already converted (no pipelined retention)

- **node_stats** — sequential BlobReader + DecompressPool (commit noted in TODO)
- **tags_count** — sequential BlobReader + DecompressPool
- **check_refs** — sequential BlobReader with new_with_scratch
- **tags_filter two-pass** — pread workers (parallel_classify_phase)
- **extract (all strategies)** — pread workers
- **sort** — sequential seek-based reads (pread per blob)
- **merge** — pread-based passthrough + sweep merge
- **diff/derive_changes** — StreamingBlocks (sequential + DecompressPool)
- **external_join** — pread workers + sequential BlobReader

## Recommendation

Convert `renumber` to sequential BlobReader + DecompressPool. This is
the only production-relevant path with unbounded retention. Done in
the same commit as this audit update.

`cat --type` does NOT need conversion — the batch pattern (BATCH_SIZE=64,
32 MB budget) bounds retention to ~3.2 MB, and the parallel batch
processing (`process_batch`) is valuable for throughput.

The remaining paths (getid, tags_filter single-pass, getparents) are
bounded by BATCH_SIZE and are acceptable.

**Note:** Converting away from pipelined decode trades I/O-decode
overlap for zero retention. At planet scale on a 30 GB host, the
retention reduction is worth the slight decode throughput loss.
On hosts with 64+ GB RAM, the pipelined reader may still be faster
since retention doesn't cause swapping.
