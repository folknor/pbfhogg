# Extract performance: findings and next steps

## Current state (commit `f28ae61`)

Sequential BlobReader for all extract strategies. Fixes Europe OOM.
Inline string table entries + pool recycling shipped in pipeline.rs
but not sufficient for extract at scale.

### Europe (32.4 GB, full-continent bbox)

| Strategy | Sequential (current) | Anon peak |
|----------|---------------------|----------|
| simple | 362s | 2.4 GB |
| complete | 553s | 4.1 GB |
| smart | 633s | 4.5 GB |

### Japan (2.4 GB, Tokyo bbox)

| Strategy | Pipelined (old) | Sequential (current) | Delta | osmium |
|----------|----------------|---------------------|-------|--------|
| simple | 11.9s | 19.8s | +66% | 7.2s |
| complete | 12.9s | 26.0s | +102% | 11.0s |
| smart | 14.4s | 31.4s | +118% | 13.4s |

## Root cause: cross-thread decompressed buffer retention

The pipelined reader's per-block lifecycle:
1. IO thread reads compressed blob → `Bytes`
2. Decode thread decompresses into pooled `Vec<u8>` → wraps as `Bytes`
3. PrimitiveBlock sent to consumer via channel
4. Consumer drops PrimitiveBlock → `Bytes` drops → pool return or cross-thread free

The `DecompressPool` holds 64 buffers. At 520K blobs (Europe full bbox),
~519K buffers overflow the pool and are freed cross-thread (~2 MB each).
glibc retains ~22-27 GB of freed pages.

## Experiments (all on Europe, full-continent bbox)

### 1. Sequential BlobReader (current, committed)
All strategies: works, 2-4 GB anon. 38-118% slower than pipelined.

### 2. decode_ahead=4
25.9 GB anon — same as default. Retention scales with total blob count, not window.

### 3. Inline string table entries (no pool)
Simple: 220s, 27.3 GB. Complete: 337s, OOM at 27.5 GB.
Eliminated Box retention (~5 GB) but decompressed Vec retention remains.

### 4. Inline entries + pool recycling (committed in pipeline.rs)
Simple: 234s, 27.2 GB. Complete: 269s, 27.5 GB. Smart: OOM at 27.6 GB.
Pool helps but 64 slots can't absorb 520K buffers. Completed on 30 GB
host but no headroom — not planet-safe (~1.4M blobs would OOM anywhere).

### 5. Hybrid: pipelined simple/complete, sequential smart
Simple: 224s. Complete: 269s. Smart: 507s.
Best performance but 27+ GB anon on simple/complete — fragile at Europe,
would OOM at planet. Reverted.

## Why osmium doesn't have this problem

osmium (libosmium + protozero) uses zero-copy decoding. String table
references are byte offsets into the decompressed buffer — no separate
Box allocation. The decompressed buffer is the only cross-thread object.
And libosmium's `osmium::memory::Buffer` pools/recycles more effectively
than our 64-slot DecompressPool.

Additionally, osmium copies raw protobuf group bytes for matching
elements instead of full decode → re-encode via BlockBuilder.

## The fix: pread-from-workers for extract

The only approach that fully eliminates cross-thread retention:

**Workers own the full lifecycle.** IO thread reads headers only. Workers
pread blob data from `Arc<File>`, decompress into thread-local buffer,
construct PrimitiveBlock, and either:
- Send classification results (compact `Vec<i64>`) for ID-collection passes
- Send `OwnedBlock` results for write passes

The decompressed buffer never crosses threads. PrimitiveBlock is created
and dropped entirely on the worker thread.

### Challenges for extract

**Classification needs shared mutable state.** Simple single-pass and
pass 1 of complete/smart mutate `IdSetDense` (bbox_node_ids,
matched_way_ids, etc.) sequentially in file order. Workers can't classify
independently.

**Possible approach:** Workers send lightweight classification results
instead of PrimitiveBlocks:
- Node blobs: `Vec<i64>` of IDs in bbox (via node-only scanner)
- Way blobs: `(way_id, has_bbox_ref: bool)` for each way
- Relation blobs: `(relation_id, has_matched_member: bool)`

Consumer updates IdSetDense from these compact results. Then for the
write pass (simple single-pass or complete/smart pass 2-3), workers
own the full decode → rewrite → OwnedBlock lifecycle since ID sets
are read-only by then.

### For complete/smart write passes

After pass 1, ID sets are read-only. Workers can do:
pread → decompress → PrimitiveBlock → filter/rewrite → OwnedBlock.
Same pattern as external join P2c. This is the highest-confidence
next step.

## Infrastructure shipped

The inline entries + pool recycling work is committed and active in
`pipeline.rs` for all pipelined commands. It reduces per-block
retention from ~50 KB (Box entries + group_ranges) to ~0 KB. This
benefits every command that uses the pipelined reader at moderate
scale. Extract is the outlier because it processes 520K+ full-size
blobs where the ~2 MB decompressed buffer retention dominates.
