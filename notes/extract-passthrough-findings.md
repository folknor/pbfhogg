# Extract performance: findings and current state

## Current state (commit `de63fd7`)

Hybrid approach: sequential BlobReader for classification passes,
pread-from-workers for write passes (complete pass 2, smart pass 3).
Simple single-pass stays sequential (classification needs shared mutable state).

### Europe (32.4 GB, full-continent bbox)

| Strategy | Time | Anon peak | Planet-safe |
|----------|------|----------|-------------|
| simple | 357s | 2.4 GB | Yes |
| complete | 382s | ~4 GB | Yes |
| smart | 441s | 4.7 GB | Yes |

### Improvement from pread-from-workers write passes

| Strategy | All-sequential | Pread write passes | Improvement |
|----------|---------------|-------------------|-------------|
| simple | 362s | 357s | — (still sequential) |
| complete | 553s | **382s** | **-31%** |
| smart | 633s | **441s** | **-30%** |

### Japan (2.4 GB, Tokyo bbox)

| Strategy | Original pipelined | Current | osmium |
|----------|-------------------|---------|--------|
| simple | 11.9s | ~20s | 7.2s |
| complete | 12.9s | ~16s (est.) | 11.0s |
| smart | 14.4s | ~18s (est.) | 13.4s |

## Architecture

**Simple (sorted single-pass):** Sequential BlobReader. Classification mutates
shared IdSetDense — can't parallelize. The full single-pass interleaves
classify + write. Pread-from-workers would require splitting into workers
sending classification results + consumer updating ID sets.

**Complete (two passes):** Pass 1 (classification) uses `collect_pass1_generic`
with sequential BlobReader. Pass 2 (write) uses **pread-from-workers**: workers
own full PrimitiveBlock lifecycle (pread → decompress → extract_block_pass2 →
OwnedBlocks). ID sets are read-only. Consumer reorders + writes.

**Smart (three passes):** Pass 1 same as complete. Pass 2 (way closure)
sequential — mutates extra_way_ids/extra_node_ids. Pass 3 (write) uses
**pread-from-workers** same as complete pass 2 but with ExtractPass3IdSets.

## Root cause investigation

The pipelined reader (`into_blocks_pipelined`) caused 25+ GB retention at
Europe scale from two sources:
1. `WireStringTable::entries: Box<[(u32,u32)]>` — cross-thread alloc/free
2. Decompressed `Bytes` buffer — cross-thread alloc/free when pool overflows

### Experiments tried

| Approach | Simple result | Complete result | Smart result |
|----------|-------------|----------------|-------------|
| Sequential BlobReader | 362s / 2.4 GB | 553s / 4.1 GB | 633s / 4.5 GB |
| decode_ahead=4 | OOM 25.9 GB | — | — |
| Inline entries (no pool) | 220s / 27.3 GB | 337s / OOM | — |
| Inline entries + pool | 234s / 27.2 GB | 269s / 27.5 GB | OOM 27.6 GB |
| Hybrid pipelined + seq | 224s / 27 GB | 269s / 27 GB | 507s mixed |
| **Pread-from-workers write** | 357s / 2.4 GB | **382s / ~4 GB** | **441s / 4.7 GB** |

### Key findings

- **decode_ahead reduction doesn't help** — retention is cumulative across
  all blobs, not bounded by the pipeline window.
- **Inline entries eliminate Box retention** (~5 GB) but the ~2 MB
  decompressed buffer retention remains dominant.
- **Pool recycling helps** but 64 slots can't absorb 520K+ blobs.
  Buffers that overflow the pool are freed cross-thread.
- **Pread-from-workers is the real fix** — workers own the buffer lifecycle.
  PrimitiveBlock created and dropped on the same thread. Only compact
  OwnedBlocks cross the thread boundary.

## Infrastructure shipped

- **Inline string table entries** (wire.rs) — `WireStringTable` and
  `group_ranges` entries appended as raw LE bytes in the decompressed
  buffer. Zero separate Box allocations. Used by pipeline.rs for all
  pipelined commands.
- **Pool-recycled inline path** (blob.rs) — `to_primitiveblock_inline(&pool)`
  uses DecompressPool for buffer recycling. On drop, Bytes returns Vec to
  pool via PooledBuffer. Benefits all pipelined commands at moderate scale.
- **Pread-from-workers for extract write passes** — complete pass 2 and
  smart pass 3. Workers own full PrimitiveBlock lifecycle. Planet-safe.

## Remaining gap vs osmium

Simple extract is the biggest gap (~2.75x at Japan). The sequential reader
is the bottleneck — no parallel decode. Fixing this requires either:
1. Pread-from-workers for classification (workers send compact results)
2. The hybrid pipeline (PipelineOutput enum in pipeline.rs)

Both are significant infrastructure changes. Complete and smart are closer
to osmium at larger scales where the multi-pass algorithm is competitive.
