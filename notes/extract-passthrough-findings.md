# Extract performance: findings and next steps

## Current state (commit `2a8a649`)

Sequential BlobReader for all extract strategies. Fixes Europe OOM (25.7 GB → 2.4-4.5 GB anon).
Japan regressed significantly from losing pipelined decode parallelism.

### Japan (2.4 GB, Tokyo bbox)

| Strategy | Pipelined (before) | Sequential (current) | Delta | osmium |
|----------|-------------------|---------------------|-------|--------|
| simple | 11.9s | 19.8s | +66% | 7.2s |
| complete | 12.9s | 26.0s | +102% | 11.0s |
| smart | 14.4s | 31.4s | +118% | 13.4s |

### Europe (32.4 GB, full-continent bbox)

| Strategy | Pipelined | Sequential | Anon peak |
|----------|----------|-----------|----------|
| simple | OOM (25.7 GB) | 362s | 2.4 GB |
| complete | would OOM | 553s | 4.1 GB |
| smart | would OOM | 633s | 4.5 GB |

## Why the pipelined reader OOMs

`WireStringTable::entries: Box<[(u32, u32)]>` — 500-2000 entries per block,
4-16 KB per allocation. Allocated on rayon decode threads, freed on consumer
thread via `batch.clear()`. glibc retains freed pages in per-thread arenas.
At 520K blocks × ~10 KB average: ~5 GB cumulative cross-thread churn.
Combined with raw blob `Bytes` retention (~16 GB), total reaches 25+ GB.

### decode_ahead=4 experiment

Reduced pipeline buffer from 32 to 4 in-flight decoded blocks. Result:
25.9 GB anon — no improvement. Confirms retention scales with total blob
count (cumulative cross-thread frees), not in-flight window.

### Why osmium doesn't OOM

osmium (libosmium + protozero) uses zero-copy decoding — string table
references are byte offsets into the decompressed buffer, no separate
`Box` allocation per block. One large allocation (decompressed buffer)
crosses threads, not hundreds of small ones. glibc handles one large
cross-thread free per block efficiently; hundreds of small ones fragment
arenas.

## Approaches tried and failed

1. **Sequential BlobReader + node-only scanner** for classification.
   Japan: 16.5s (+39%). Lost decode parallelism.

2. **Raw-frame reader + blob-level passthrough** for fully-contained nodes.
   Japan: 20.4s (+72%). Lost all decode parallelism.

3. **Reduced decode_ahead (4 instead of 32)**.
   Europe: 25.9 GB, still OOM. Retention is cumulative, not bounded by window.

## Viable next approaches (priority order)

### 1. Eliminate cross-thread allocation in WireStringTable (best fix)

The root cause is `entries: Box<[(u32, u32)]>` allocated on decode threads.
Three sub-approaches:

**a) Inline entries into the decompressed buffer.** During `WireBlock::parse`,
append the `(u32, u32)` entry pairs after the protobuf data in the same `Vec<u8>`.
`WireStringTable` stores `(entries_offset: u32, entries_count: u32)` — 8 bytes
fixed, zero separate heap allocation. `get()` reads entries from the buffer.

Requires: `WireBlock::parse` takes `&mut Vec<u8>` instead of `&[u8]`.
The buffer grows by ~4-16 KB per block (the entries data). This is fine — the
buffer is 1-2 MB already. Total overhead: <1%.

`group_ranges` can use the same approach (append to buffer), or switch to
`SmallVec<[(u32, u32); 8]>` (inline, no heap, 99% of blocks have ≤4 groups).

Impact: eliminates ALL cross-thread Box allocations in PrimitiveBlock.
The pipelined reader becomes safe at any scale. Every command benefits.

**b) Vec pool for entries.** `DecompressPool`-style shared pool for
`Vec<(u32, u32)>`. Decode workers take from pool, consumer returns.
Same pattern as P2b tuple pool. Less architectural change than (a) but
still has cross-thread Vec ownership (pool mitigates but doesn't eliminate).

**c) Thread-local entries Vec via `thread_local!` in decode workers.**
Workers reuse a thread-local Vec for entries, `mem::take` to move into
WireStringTable. Consumer drops the taken Vec (cross-thread free of the
Vec itself, but only one per block instead of one per string table).
Simpler than (b) but still one small cross-thread free per block.

### 2. Pread-from-workers for extract write passes

For complete/smart: after pass 1 builds ID sets, the write pass can use
P2c pattern — workers own full PrimitiveBlock lifecycle (pread → decompress
→ rewrite → OwnedBlock). ID sets are read-only during the write pass.

For simple single-pass: harder — classification mutates shared IdSetDense
during the scan. Workers can't classify. Would need either (a) the
WireStringTable fix from approach 1, or (b) workers send classification
results (compact Vec<i64>) instead of PrimitiveBlocks.

### 3. Hybrid pipeline (PipelineOutput enum)

`PipelineOutput::Decoded(PrimitiveBlock) | Passthrough(RawBlobFrame)`.
Pipeline decides per-blob. Fully-contained node blobs skip decode entirely.
Most impactful for simple extract with spatial selectivity.

Requires approach 1 or 2 to be safe — the `Decoded` variant still has
the cross-thread PrimitiveBlock problem. The hybrid pipeline optimizes
the passthrough portion but doesn't fix the decode retention.

## Recommendation

**Approach 1a (inline entries) is the real fix.** It eliminates the root
cause at the wire-format level. Every command benefits automatically —
no per-command pipeline changes needed. The pipelined reader becomes
planet-safe. Extract recovers its pipelined performance. The osmium gap
closes because we're no longer paying for cross-thread Box fragmentation.

Estimated effort: ~100 lines in wire.rs + block.rs. The `Bytes::from(vec)`
ownership model already gives the consumer a mutable-ish buffer. The
parse just needs to write the entries table into the same buffer.
