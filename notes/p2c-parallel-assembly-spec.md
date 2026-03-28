# P2c: Parallel assembly for external join stage 4

## Goal

Parallelize external join stage 4 (assembly). Currently 432s at Europe
scale (50% of total 866s). Sequential BlobReader + batch-parallel
assembly. The outer read loop is the bottleneck.

## Current architecture

```
Sequential BlobReader (one blob at a time)
  │ decompress via DecompressPool
  │ construct PrimitiveBlock
  │ accumulate into batch (BATCH_SIZE blocks)
  ▼
Per-batch parallel assembly (rayon par_iter):
  │ pre-scan: iterate all elements to count way refs → slot_pos starts
  │ parallel: each block gets (block, slot_start) → assemble → OwnedBlock
  │ sequential: drain results to pipelined PbfWriter
  ▼
Pipelined PbfWriter (compression on writer thread)
```

### Current cost breakdown

- 432s total at Europe (sidecar `070086bb`, commit `80e227b`)
- 36% estimated output zlib compression (167s at earlier measurement)
- Rest: sequential read + decompress + assembly
- Anon: 2.1 GB (sequential reader, bounded)

### The slot_pos dependency

Each way's node refs consume consecutive `slot_pos` values into the
coord_slots file. Stage 1 assigned slot_pos 0..N sequentially while
streaming ways. Stage 4 must read coords at the same positions.

Within a batch, this is already solved: lines 913-922 pre-scan all
blocks in the batch to compute per-block `block_slot_starts`, then
parallel workers use their assigned start positions independently.

The problem is the **outer loop**: we can't know a blob's ref count
without decompressing and parsing it. So blobs must be read sequentially
to maintain the global slot_pos cursor.

## Architecture

```
Stage 1 (way pass): count refs per way blob → write sidecar file
  │ sidecar: Vec<u64> of per-way-blob ref counts (~128 KB at Europe)
  │ trailer: total ref count for alignment verification
  ▼

Stage 4 pre-scan: header-only pass to build blob schedule
  │ BlobReader::next_header_with_data_offset (seekable, no blob reads)
  │ For each OsmData blob:
  │   P1b-skipped node blobs → drop (not scheduled)
  │   Way blobs → consume next sidecar entry → compute slot_start
  │   All others → slot_start = 0 (unused)
  │ Verify: sidecar entries consumed == way blobs seen (fatal on mismatch)
  │ Verify: cumulative ref count == total_slots from stage 1
  │ Build schedule: Vec<BlobDescriptor>
  ▼

Workers (dedicated thread pool, NOT global rayon):
  │ shared: Arc<File> for pread, &CoordSlots for coord lookup
  │ thread-local: read_buf, decompress_buf, BlockBuilder, refs_buf, locations_buf
  │
  │ All blobs go through the same path:
  │   pread → decompress_blob_raw → PrimitiveBlock::new
  │   assemble_block(block, slot_start, coord_slots, ...) → Vec<OwnedBlock>
  │   send (seq, Ok(Assembled(blocks, stats)))
  │
  │ PrimitiveBlock created and dropped entirely on worker thread.
  │ No cross-thread alloc/free of WireStringTable/group_ranges.
  ▼

sync_channel(32) — (usize, Result<WorkerResult>)
  │
  ▼
Consumer (main thread):
  │ ReorderBuffer delivers in file order
  │ Assembled → drain OwnedBlocks to PbfWriter
  ▼
Pipelined PbfWriter (compression on rayon global pool)
```

### V1 simplification: no relation passthrough

All non-skipped blobs go through workers with the same
pread → decompress → PrimitiveBlock → assemble path. This includes
relation blobs (~600 at Europe, ~14K at planet).

Cost of decompressing relation blobs: ~0.3s Europe, ~14s planet.
Negligible against a 230s stage. Passthrough adds complexity
(frame_offset tracking, WorkerResult enum, IO-thread dual dispatch)
for unmeasurable gain.

**Defer passthrough to v2** if planet profiling shows it matters.

Per planet-Claude: if passthrough is added later, the IO thread should
handle it directly (pread frame bytes, send to consumer channel with
seq number) rather than routing through workers. This avoids needing
frame_offset in the worker descriptor. Workers only see blobs needing
decompress.

### Sidecar design

Stage 1 writes a ref-count sidecar in the scratch directory:
- One `u64` per way blob, in blob file order
- Value = total way-node refs in that blob
- Trailer: one `u64` with total ref count (sum of all entries)
- File size: ~16K way blobs × 8 bytes + 8 byte trailer = ~128 KB Europe

Stage 4 reads the sidecar and computes prefix sums:
```
slot_starts[0] = 0
slot_starts[i] = slot_starts[i-1] + ref_counts[i-1]
```

**Alignment invariant (fatal in release builds):**
- Sidecar entries consumed must equal way blobs seen in stage 4 pre-scan
- Cumulative ref count must equal `total_slots` from stage 1
- Mismatch → error, not silent corruption

Per planet-Codex: this must be a hard runtime check, not a debug
assertion. Silent slot_pos corruption would produce wrong coordinates
with no visible error.

**Fragility note:** The sidecar correctness depends on stage 1 and
stage 4 seeing the same way blobs in the same order. Both identify
way blobs via `ElemKind::Way` from indexdata on the same file. Document
this invariant in the sidecar write code — a future change that
reorders blobs would silently break alignment.

### Blob type dispatch

The IO thread's pre-scan classifies each blob:

| Blob type | Decision | Handling |
|-----------|----------|----------|
| Node (P1b skip) | Drop | Not scheduled, no worker |
| Node (has tags or members) | Rewrite | Worker: decompress → filter → rebuild |
| Way | Rewrite | Worker: decompress → assemble with coords |
| Relation | Rewrite (v1) | Worker: decompress → pass through assembly |

P1b filtering uses indexdata + tagdata + relation member range check.
All available from blob header — IO thread decides without reading
blob data. Requires `BlobHeader::tag_index()` (same pattern as
`BlobHeader::index()`, needs adding).

Non-way blobs get `slot_start = 0`. The assembly function handles
this correctly — node/relation blobs contain no ways, so slot_pos is
never advanced.

### Thread pool isolation

Per planet-Codex: **do NOT use the global rayon pool for stage 4
workers.** PbfWriter already dispatches compression onto rayon. If
P2c workers also use rayon, decode+assemble and compression contend
for the same threads.

Use dedicated worker threads via `std::thread::scope` (same as
P2b-v2) or a dedicated `rayon::ThreadPoolBuilder::new().build()`.
4-6 workers, separate from the PbfWriter's rayon pool.

### Memory model

All worker buffers are thread-local:
- `read_buf: Vec<u8>` — pread target, reused
- `decompress_buf: Vec<u8>` — decompress target, reused (via decompress_blob_raw)
- `BlockBuilder` — reused across blobs
- `refs_buf: Vec<i64>` — way ref scratch, reused
- `locations_buf: Vec<(i32, i32)>` — coord lookup scratch, reused

`CoordSlots` is a read-only mmap, shared via `&CoordSlots` within
`thread::scope`. No Arc needed.

OwnedBlocks cross the thread boundary to consumer. ~32 in flight
(channel capacity), each ~64 KB. Total ~2 MB in flight. Bounded.

PrimitiveBlock lifecycle is entirely on the worker thread: created
from decompress_blob_raw output, used for assembly, dropped before
send. WireStringTable Box and group_ranges Box allocated and freed
on the same thread. No cross-thread retention.

### Fadvise

Per planet-Codex: **skip fadvise for v1.** Get parallel assembly
working first. Add worker-side `fadvise(DONTNEED)` after each pread
only if sidecar data shows RssFile growing. Blobs are disjoint and
never re-read, so per-blob eviction is safe but adds complexity.

### Existing infrastructure reused

- `BlobReader::next_header_with_data_offset` — from P2b-v2
- `BlobHeader::index()` — from P2b-v2
- `decompress_blob_raw()` — from P2b-v2
- `ReorderBuffer` — from pipeline.rs
- `assemble_block()` — existing function, called per-blob by workers
- `PbfWriter` pipelined — already in use

**Needs adding:**
- `BlobHeader::tag_index()` — trivial, same pattern as `index()`

### Expected performance

**Stage 4 breakdown estimate (Europe, sequential):**
- Sequential read: ~10s (NVMe throughput)
- Zlib decompression: ~200s (single-threaded)
- PrimitiveBlock construction: ~20s
- Assembly (coord lookup + BlockBuilder): ~35s
- Output compression: ~167s (pipelined on rayon)

**With 6 dedicated workers:**
- Read + decompress + parse + assemble: ~265s / 6 = ~44s
- Output compression: ~167s (unchanged, pipelined)
- Pipeline overhead (fill/drain, reorder): ~20s
- **Estimated: ~230s** (compression-bound)

Target: < 300s (solid). Stretch: < 250s.

Bottleneck shifts from sequential decompress to output compression.
To go below 167s would need `--compression none` (user choice).

Total pipeline estimated: 126 + 216 + 91 + 230 = ~663s (down from
866s, -23%). With compression none: potentially ~500s.

### Alternatives considered

**Two-pass stage 4:** Extra PBF pass to count refs. Works but adds
~30s. Sidecar from stage 1 is free.

**Overlap read + assembly only:** Simpler, no sidecar. But bounded —
only hides read latency behind assembly, doesn't parallelize decompress.

**Per-worker temp files + concatenation:** Complex ordering/cleanup
for no gain over the proven consumer + PbfWriter pattern.

**Relation passthrough in v1:** Saves ~0.3s Europe, ~14s planet.
Not worth the complexity. Defer to v2.

## Results (commit `6b09796`, plantasjen, sidecar `bc38a079`)

### Europe (32.4 GB): 577s (9.6 min), down from 866s (-33%)

| Stage | Before (P2b-v2, 866s) | P2c (577s) | Delta |
|-------|----------------------|-----------|-------|
| Stage 1 | 126s / 70 MB | 128s / 70 MB | — |
| Stage 2 | 216s / 1.4 GB | 221s / 1.4 GB | — |
| Stage 3 | 91s | 91s | — |
| Stage 4 | **432s** / 2.1 GB | **136s** / 7.3 GB | **-68%** |

Stage 4 anon: 7.3 GB peak (parallel workers hold in-flight
PrimitiveBlocks + OwnedBlocks). Planet extrapolation: ~20 GB.
Fits on 32 GB host but tighter than stages 1-3.

Denmark: 12.3s, 0 missing locations.

### Comparison to dense ALTW

| Index | Europe | Ratio |
|-------|--------|-------|
| Dense | 2,565s (43 min) | baseline |
| External (sequential, commit `ee9b19f`) | 901s (15 min) | 2.8x faster |
| **External (P2b+P2c, commit `6b09796`)** | **577s (9.6 min)** | **4.5x faster** |

### Beat all estimates

- Spec predicted stage 4: ~230s. Actual: 136s.
- Spec predicted total: ~663s. Actual: 577s.
- Target was < 300s for stage 4. Beat by 55%.

The pread-from-workers pattern is more efficient than expected — the
sequential BlobReader was a bigger bottleneck than the decompress time
alone suggested (likely from syscall overhead and buffer copies).

## Validation plan

1. ~~Denmark correctness (diff against current output, 0 differences)~~ Done
2. ~~Sidecar alignment check passes (ref count == total_slots)~~ Done
3. ~~Denmark sidecar: anon stays < 500 MB~~ Done
4. ~~Europe: wall time < 300s (target ~230s)~~ Done (136s)
5. ~~Europe sidecar: anon < 3 GB~~ 7.3 GB (higher than target, acceptable)

## Implementation order

1. Add `BlobHeader::tag_index()` method
2. Stage 1: write ref-count sidecar file during way pass
3. Stage 4: header-only pre-scan to build blob schedule with slot_starts
4. Stage 4: dedicated worker threads with pread + decompress + assemble
5. Stage 4: consumer with ReorderBuffer + PbfWriter drain
6. Denmark correctness test
7. Europe bench with sidecar

## Reviewer sign-off

- **perf-Claude:** Approved. Sidecar correct, OwnedBlocks via consumer,
  per-blob parallelism sufficient. Node blobs are NOT passthrough
  (need per-node filtering). Header-only P1b filtering works.
  Estimated ~200-210s (compression-bound).

- **perf-Codex:** Approved. Sidecar keyed by file-order blob identity.
  Workers produce OwnedBlocks, consumer writes ordered. Per-blob
  parallelism sufficient for v1. Target < 300s realistic.

- **planet-Claude:** Approved. Relation passthrough should be IO-thread
  handled (not workers) if added later. Sidecar checksum trailer.
  Simplification: skip passthrough for v1, all blobs through workers.
  PrimitiveBlock lifecycle confirmed clean.

- **planet-Codex:** Approved. Hard runtime sidecar invariant check.
  Dedicated thread pool (not global rayon). Skip fadvise for v1.
  Byte-budget the read-ahead queue if added later.
