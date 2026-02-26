# Merge architecture

## Overview

`src/commands/merge.rs` applies an OSC diff (create/modify/delete changes)
to a sorted base PBF, producing an updated sorted PBF. The key design goal
is to avoid re-encoding blobs that aren't affected by the diff — most blobs
are passed through as raw bytes.

## Data structures

### DiffOverlay (from `src/osc.rs`)

Parsed OSC file. Contains:
- `nodes: HashMap<i64, OscNode>` — creates and modifies
- `ways: HashMap<i64, OscWay>` — creates and modifies
- `relations: HashMap<i64, OscRelation>` — creates and modifies
- `deleted_nodes/ways/relations: HashSet<i64>` — deletes

### DiffRanges

Pre-sorted ID vectors for fast coarse overlap checking. Built once from
`DiffOverlay`. Includes both upserts and deletes. `range_overlaps(kind,
min_id, max_id)` uses binary search to check if any affected ID falls
within a blob's range — O(log n) per check.

### SkipState

Tracks whether all element types have passed their max affected ID. Once
`all_done()` returns true, remaining blobs skip decompression entirely.
This is the fast-exit path for the tail of large PBFs where the diff only
touches low-numbered elements.

### RawBlobFrame

A raw blob frame: the complete `[4-byte len][BlobHeader][Blob]` bytes.
The Blob protobuf bytes are a suffix of `frame_bytes` starting at
`blob_offset`, eliminating a separate ~55 KB allocation per blob.
Optionally carries a `BlobIndex` from BlobHeader indexdata for fast
classification without decompression.

### CreateEmitter

Cursor-based sorted create emitter. Holds sorted diff IDs per type
(all of `diff.nodes.keys()`, `diff.ways.keys()`, `diff.relations.keys()`).
Maintains per-type cursors that advance monotonically.

`emit_before(kind, min_id)` emits all creates with ID < min_id for the
given type — these are diff IDs NOT in the `emitted_*` sets (i.e. pure
creates, not modifications of existing base elements). Handles type
transitions: when switching from Node to Way, flushes all remaining node
creates first.

Used in the sequential output phase (Phase 4) for creates that fall between
blobs. Creates within rewrite blobs are pre-assigned and interleaved during
Phase 3 (see below).

`creates_in_range(kind, first_id, last_id, emitted)` returns sorted diff
IDs in `[first_id, last_id]` not in the emitted set — used during Phase 2
to pre-assign creates to rewrite blobs.

### Emitted sets

Three `HashSet<i64>`: `emitted_nodes`, `emitted_ways`, `emitted_relations`.
Contain diff IDs that have already been written to the output — either as
modifications of base elements (discovered in Phase 2 via
`collect_modifications`) or as creates interleaved in rewrite blobs (added
in Phase 2 after `creates_in_range`). `CreateEmitter` checks these sets
to avoid double-emission.

## Blob classification

Two-stage classification determines whether a blob needs rewriting:

### Stage 1: Coarse range check (`classify_only`, parallel)

For each blob in the batch:
1. **Index fast path**: If the blob has inline indexdata (`BlobHeader`
   indexdata field, 26 bytes), check `DiffRanges::range_overlaps` directly.
   No decompression needed. Returns `Passthrough(BlobIndex)`.
2. **Slow path**: Decompress blob, run `scan_block_ids` (lightweight
   protobuf scan — extracts element type + ID range without full parse).
   If no overlap, returns `Passthrough(BlobIndex)`.
3. **MayOverlap**: Range overlaps. Full parse via
   `parse_primitive_block_from_bytes_owned`, then Stage 2.

### Stage 2: Precise element-level check (`block_overlaps_diff`)

Iterates actual element IDs in the parsed block and checks each against the
diff's HashMap/HashSet. If no element is affected, returns `FalsePositive`.
This catches the case where the diff only has pure creates with IDs in the
blob's range — no base element needs rewriting, so the blob is passed
through raw. The creates are emitted at block boundaries by `CreateEmitter`.

If any element IS affected, returns `NeedsRewrite` with the parsed
`PrimitiveBlock`, element kind, and ID range `[first_id, last_id]`.

## Batch pipeline (4 phases)

Blobs are processed in batches of 64 (`BATCH_SIZE`). Each batch goes through
4 phases. Cross-batch ordering is sequential (batch N completes before
batch N+1 starts).

```
  ┌─────────────────────────────────────────────────────────────────┐
  │ Batch (64 blobs)                                                │
  │                                                                 │
  │  Phase 1: Parallel classify                    [rayon pool]     │
  │  ┌──────┐ ┌──────┐ ┌──────┐     ┌──────┐                      │
  │  │blob 0│ │blob 1│ │blob 2│ ... │blob63│  → Vec<ClassifyResult>│
  │  └──────┘ └──────┘ └──────┘     └──────┘                       │
  │                                                                 │
  │  Phase 2: Sequential pre-compute               [main thread]   │
  │  For each NeedsRewrite:                                        │
  │    • collect_modifications → emitted_* sets                    │
  │    • creates_in_range → create assignment                      │
  │    • add creates to emitted_* sets                             │
  │  Build: Vec<BatchSlot> + Vec<rewrite_jobs>                     │
  │                                                                 │
  │  Phase 3: Parallel rewrite                     [rayon pool]     │
  │  ┌───────────┐ ┌───────────┐     ┌───────────┐                │
  │  │rewrite job│ │rewrite job│ ... │rewrite job│                 │
  │  │+ creates  │ │+ creates  │     │+ creates  │                 │
  │  └───────────┘ └───────────┘     └───────────┘                 │
  │  → Vec<RewriteOutput>                                          │
  │                                                                 │
  │  Phase 4: Sequential output                    [main thread]   │
  │  For each blob in file order:                                  │
  │    Passthrough → emit_before + write raw frame                 │
  │    FalsePositive → emit_before + write raw frame               │
  │    Rewrite → emit_before + write pre-serialized blocks         │
  │                                                                 │
  └─────────────────────────────────────────────────────────────────┘
```

### Phase 1: Parallel classify

```rust
batch.par_iter().map_init(Vec::new, |buf, frame| {
    classify_only(frame, &ranges, &diff, buf)
})
```

Each rayon worker gets a reusable decompression buffer (`Vec::new` via
`map_init` — stack-only until first use, so rayon re-initing under
work-stealing costs nothing). Workers decompress, scan, parse overlapping
blobs, and return `ClassifyResult`. Non-overlapping blobs return
`Passthrough` without parsing.

### Phase 2: Sequential pre-compute

Runs on the main thread. For each `NeedsRewrite` blob:

1. **Collect modifications**: `collect_modifications` walks the parsed
   block's elements and adds IDs found in `diff.nodes/ways/relations` to
   the `emitted_*` sets. These are base elements being replaced by diff
   versions — marking them prevents `CreateEmitter` from re-emitting them
   as creates.

2. **Assign creates**: `create_emitter.creates_in_range(kind, first_id,
   last_id, emitted)` uses binary search (`partition_point`) on the sorted
   diff ID list to find creates in this blob's range that aren't
   modifications. Returns a sorted `Vec<i64>`.

3. **Mark creates as emitted**: Add the assigned create IDs to the
   `emitted_*` set so `CreateEmitter` won't double-emit them in Phase 4.

This phase also builds:
- `slots: Vec<BatchSlot>` — one per blob, records whether it's
  `Passthrough`, `FalsePositive`, or `Rewrite { job_index, kind,
  first_id, last_id }`
- `rewrite_jobs: Vec<(&PrimitiveBlock, ElemKind, Vec<i64>)>` — blocks
  and their pre-assigned creates, ready for Phase 3

### Phase 3: Parallel rewrite

```rust
rewrite_jobs.par_iter().map_init(BlockBuilder::new, |bb, (block, kind, creates)| {
    rewrite_block_parallel(block, &diff, bb, creates, *kind)
})
```

Each rayon worker gets a reusable `BlockBuilder` (~48 KB heap) via
`map_init`. For each rewrite job:

1. `bb.pre_seed_string_table(block)` — copies the base block's string
   table so raw string indices remain valid in the output.
2. Iterate elements in the base block. For each element:
   - Emit pre-assigned creates with ID < element ID (via `create_cursor`).
   - Check if the element is deleted → skip.
   - Check if the element is modified → write diff version.
   - Otherwise → pass through raw bytes (`write_base_*_local`).
3. Emit remaining creates after the last element.
4. `flush_local` → collect serialized block bytes.

Output: `RewriteOutput { blocks: Vec<Vec<u8>>, stats: MergeStats }`.

The `_local` helpers (`flush_local`, `ensure_*_capacity_local`,
`write_base_*_local`, `emit_create_local`) mirror the writer-backed
versions but flush to a local `Vec<Vec<u8>>` instead of `PbfWriter`.

### Phase 4: Sequential output

Main thread iterates `slots` in file order:

- **Passthrough**: `create_emitter.emit_before(kind, min_id, ...)` emits
  any creates with ID < this blob's minimum, then the raw frame is written
  via `write_passthrough` (or `reframe_raw_with_index` if the blob lacks
  indexdata).
- **FalsePositive**: Same as passthrough — the blob was decompressed and
  parsed but had no actual overlap.
- **Rewrite**: `create_emitter.emit_before(kind, first_id, ...)` emits
  creates before this blob's range, then the pre-serialized blocks from
  `RewriteOutput` are written via `writer.write_primitive_block`.

The pipelined writer (`PbfWriter::to_path_pipelined`) handles compression
on rayon workers asynchronously. Phase 4 feeds it blocks and they get
compressed in parallel with the next batch's Phase 1.

## Create interleaving: correctness argument

The key invariant: every diff ID is written to the output exactly once.

- **Modifications** (diff ID exists as a base element): Written during
  Phase 3 rewrite as a replacement. Added to `emitted_*` in Phase 2 via
  `collect_modifications`. `CreateEmitter` skips them.
- **Creates in rewrite blobs** (diff ID in `[first_id, last_id]`, not a
  modification): Pre-assigned in Phase 2 via `creates_in_range`.
  Interleaved at exact sorted position during Phase 3 by `create_cursor`.
  Added to `emitted_*` in Phase 2. `CreateEmitter` skips them.
- **Creates between blobs**: Emitted by `CreateEmitter::emit_before` in
  Phase 4 at the correct sorted position (before the next blob with
  higher min_id).
- **Creates within passthrough blobs**: Deferred to the next blob boundary
  by `CreateEmitter` (the blob's elements aren't decoded, so there's
  nowhere to interleave). Appear slightly out of strict sorted order.
  Accepted trade-off — OSM consumers tolerate this.
- **Deletes**: Skipped during Phase 3 rewrite (element omitted from
  output). Never in `CreateEmitter`'s ID lists (those come from
  `diff.nodes/ways/relations.keys()`, not `diff.deleted_*`).

## Thread model

All parallelism uses the rayon global pool. No dedicated thread pools are
created for merge. The same pool handles:
- Phase 1 classify (decompress + scan + parse)
- Phase 3 rewrite (re-encode blocks with creates)
- Pipelined writer compression (`frame_blob`)

At 12 cores (Ryzen 9 5900X), benchmarks show 5 active workers. With zlib
compression, workers handle both rewrite and compression tasks concurrently
(80% utilization). With `Compression::None`, workers mostly only do rewrite
work (29-78% utilization, less overlap opportunity).

The main thread runs Phases 2 and 4 (sequential pre-compute + output), plus
batch I/O (`read_raw_frame`). With parallel rewrite, the main thread is no
longer the bottleneck — it spends most time waiting for the rayon pool.

## Passthrough optimizations

Most blobs pass through without re-encoding. Several optimizations
minimize the cost:

- **Indexdata fast path**: 26-byte index in BlobHeader enables classify
  without decompression. Binary search on `DiffRanges` — O(log n).
- **`write_passthrough` with `copy_file_range`**: When the `linux-direct-io`
  feature is enabled and the output isn't O_DIRECT, uses kernel-space
  `copy_file_range` to copy the blob directly from input fd to output fd
  without userspace buffers.
- **`write_raw_owned`**: For non-copy-range paths, moves the `Vec<u8>` into
  the writer channel via `std::mem::take` — zero-copy transfer.
- **`RawBlobFrame` stores `blob_offset`**: Blob data is read directly into
  `frame_bytes` at the correct offset. No separate `blob_bytes` allocation
  (~55 KB saved per blob).
- **SkipState fast exit**: Once all types pass their max affected ID, entire
  batches skip decompression. At Denmark scale with a daily diff, ~0% of
  blobs hit this (the diff touches IDs across the full range). At planet
  scale with a regional diff, this could skip a large tail.

## Rewrite optimizations

For blobs that need re-encoding:

- **Pre-seeded string table**: `bb.pre_seed_string_table(block)` copies
  the base block's string table into the output `BlockBuilder`. Raw string
  indices from base elements remain valid — no per-element string table
  lookup or insertion.
- **Raw bytes passthrough**: `write_base_*_local` uses `add_node_raw`,
  `add_way_raw_bytes`, `add_relation_raw_bytes` — copies raw protobuf
  field bytes directly instead of decoding and re-encoding each field.
- **Direct wire-format encoding**: Ways and relations are encoded directly
  to protobuf wire format using reusable scratch buffers in `BlockBuilder`.
  No per-element allocation.
- **Per-thread BlockBuilder reuse**: rayon's `map_init(BlockBuilder::new)`
  reuses the `BlockBuilder` across rewrite jobs on the same thread. The
  internal `Vec`s retain capacity across calls.

## Performance characteristics

### Scaling with rewrite fraction

| Dataset  | Size  | Rewrite % | Wall (zlib) | Notes                              |
|----------|-------|-----------|-------------|------------------------------------|
| Denmark  | 465MB | 8.5%      | 3.31s       | Too few rewrites to show speedup   |
| Germany  | 4.5GB | 18.4%     | 35.1s       | -30% vs sequential (was 49.9s)     |
| Planet*  | 75GB  | ~92%      | ~10 min*    | Extrapolated from Germany numbers  |

*Planet numbers are extrapolated. At 92% rewrite, the parallel rewrite
is transformative — sequential would be ~30 min.

### Bottleneck analysis

| Config              | Bottleneck (sequential)     | Bottleneck (parallel)                 |
|---------------------|-----------------------------|---------------------------------------|
| indexdata + zlib    | frame_blob/zlib compression | frame_blob/zlib compression (deeper)  |
| indexdata + none    | rewrite_block (49%)         | rewrite_block_parallel across workers |
| no indexdata + zlib | classify_blob decompress    | classify + compression overlap        |

With zlib, the compression pipeline is always the wall-time bottleneck —
parallel rewrite helps by reducing the main thread's serial work so it can
feed the compression pipeline faster. With `Compression::None`, there's no
compression to overlap with, so the improvement is smaller (-11% at Germany
vs -30% for zlib).

### Memory

RSS is bounded regardless of file size:
- Denmark (465 MB): 132 MB
- Germany (4.5 GB): 353 MB

The dominant memory consumers:
- `DiffOverlay`: ~32 MB for a planet daily diff (~4M changes)
- `emitted_*` HashSets: proportional to rewrite fraction
- Per-thread `BlockBuilder`: ~48 KB × num_workers
- `RawBlobFrame` batch: 64 × ~64 KB = ~4 MB
- Pipelined writer buffers: bounded by channel capacity

Allocation churn is high (931 MB for Denmark merge) but RSS stays low
because most allocations are short-lived (decompress buffer, serialized
blocks, compression output).
