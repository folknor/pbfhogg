# Consolidation Review #3: Unified Blob Pipeline

## Verdict: DO NOT DO

## Inventory of Raw Frame / Blob Pipeline Implementations

### A. `src/commands/mod.rs` -- Shared Infrastructure

Already provides shared primitives:

| Primitive | Used by | Purpose |
|---|---|---|
| `RawBlobFrame` (struct) | merge, cat, add-locations | Complete frame bytes + parsed blob_type, index, tagdata, file_offset |
| `read_raw_frame()` | merge, cat, add-locations, inspect, `has_indexdata()` | Sequential raw frame reading with `file_offset` tracking |
| `read_blob_header_only()` | inspect | Header-only read (skip blob data) |
| `flush_passthrough_buf()` | merge, add-locations | Flush coalesced passthrough via `write_raw_owned()` |
| `flush_block()` | merge, sort, cat (indirectly) | Flush BlockBuilder to PbfWriter |
| `flush_local()` | merge, cat, add-locations | Flush BlockBuilder to Vec<OwnedBlock> (rayon workers) |
| `drain_batch_results()` | cat, add-locations | Sequential write of parallel batch results |
| `for_each_primitive_block_batch()` | cat (filtered), tags_filter, getid | Batched PrimitiveBlock consumption from iterator |
| `BATCH_BYTE_BUDGET`, `BATCH_MIN/MAX_BLOBS` | merge, add-locations | Adaptive batch sizing constants |

### B. cat.rs -- Simplest Pipeline

Two entirely separate paths:

**Path 1: `cat_passthrough` (no type filter)**
- Sequential `read_raw_frame()` loop, per-blob `write_raw_copy` or `write_raw_owned`. NO coalescing.
- Multi-file input. Uses `copy_file_range` per blob (not coalesced ranges).

**Path 2: `cat_filtered` (with type filter)**
- `ElementReader::open().with_blob_filter().into_blocks_pipelined()` -- library's pipelined decoder.
- Element-level filtering within blocks. Standard parallel decode -> rebuild pattern.

### C. add_locations_to_ways.rs -- Two-Phase Read + Hybrid Pipeline

Most complex pipeline architecture:

**Pass 1 (node index build):**
- Standard pipelined path, parallel batch insert into `DenseMmapIndex`.

**Pass 2a: `write_output_decode_all` (no indexdata -- fallback)**
- Standard pipelined path with `for_each_primitive_block_batch`.

**Pass 2b: `write_output_passthrough` (indexdata present -- optimized)**
- Custom two-phase read with PRIVATE `BlobHeaderInfo`, `read_blob_header()`, `read_blob_data()`, `skip_blob_data()`.
- Classification via indexdata `ElemKind`.
- Passthrough: coalescing buffer + `flush_passthrough_buf()`, OR `CopyRange` coalescing (linux-direct-io).
- Decode: `BatchSlot` enum -> byte-budgeted batch -> rayon with per-worker `DecompressPool`.
- **Unique:** Two-phase read avoids reading blob bytes for passthrough blobs. Per-worker `DecompressPool`. `CopyRange` struct for contiguous kernel-space copy.

### D. sort.rs -- Random-Access, No Streaming Pipeline

Fundamentally different architecture -- NOT a streaming pipeline:

**Pass 1 (build blob index):**
- Custom manual loop with `parse_blob_header_with_index()` + `reader.skip()` or `reader.read_exact()`. Does NOT use `read_raw_frame()`.

**Pass 2 (write in sorted order):**
- `File::open()` + `seek()` + `read_exact()` -- random access. NOT sequential streaming.
- Per-blob `write_raw_copy` or `write_raw()`. NO coalescing.
- Overlap runs decoded via `decode_blob_to_primitiveblock()` + sweep merge via `BinaryHeap`. Sequential, NOT parallelized.

### E. merge.rs -- Most Sophisticated Pipeline

Multi-phase streaming batch pipeline:

- **Read:** Dedicated reader thread streaming `RawBlobFrame` via bounded `mpsc::sync_channel(128)`.
- **Batch collection:** `collect_batch()` with byte budget estimation via `estimate_blob_cost()`.
- **Phase 1 (classify):** Rayon `par_iter().map_init(Vec::new)` calling `classify_only()`.
- **Phase 2 (assign):** Sequential. Builds `BatchSlot` vec + `RewriteJob` vec.
- **Phase 3 (parallel rewrite):** Rayon `spawn()` per job with per-task `BlockBuilder`.
- **Phase 4 (output):** Main thread iterates slots in file order. Passthrough coalescing or `write_raw_copy`. Rewrite receive from channel.
- **Unique:** Dedicated reader thread, multi-phase classification, inline upsert interleaving, gap-create emission, streaming rewrite results via channel.

## Similarity Analysis

| Feature | cat (pass) | cat (filter) | add-loc (decode-all) | add-loc (passthrough) | sort | merge |
|---|---|---|---|---|---|---|
| Read mechanism | `read_raw_frame` | `ElementReader` pipeline | `ElementReader` pipeline | Custom two-phase read | Custom seek-read | Reader thread + `read_raw_frame` |
| Classification | Trivial | `BlobFilter` | None | `ElemKind` from indexdata | Offline overlap detect | 3-level: index/scan/parse |
| Passthrough | Per-blob raw | None | None | Coalesce + CopyRange | Per-blob raw | Coalesce + CopyRange |
| Decode parallelism | None | rayon batch | rayon batch | rayon batch + DecompressPool | None (heap merge) | rayon spawn per job |
| Batch sizing | None | Fixed BATCH_SIZE | Fixed BATCH_SIZE | Byte budget | None | Byte budget |
| Output ordering | Sequential | Batch-ordered via drain | Batch-ordered via drain | Interleaved pass/decode | Sorted | Interleaved pass/decode + channel reorder |
| copy_file_range | Per-blob | No | No | CopyRange coalesced | Per-blob | Per-blob |

## Assessment

### How similar are these really?

**Not as similar as the deep-dive suggests.** The four commands share only three low-level primitives that are already factored out: `read_raw_frame`, `flush_passthrough_buf`, and `flush_block/flush_local`.

1. **Sort uses random access, not streaming.** Any streaming `FrameSource` abstraction would be useless for sort's pass 2.
2. **Cat's passthrough is trivially simple.** 10 lines of code. Wrapping it adds complexity without benefit.
3. **Add-locations has a unique two-phase read optimization** that avoids reading blob data for passthrough blobs.
4. **Merge has the most complex pipeline** deeply intertwined with diff overlay semantics.
5. **The two `coalesce_passthrough` implementations are genuinely different.** Add-locations just extends raw frame bytes. Merge conditionally re-frames non-indexed blobs with indexdata.

### Is the "common FrameSource + passthrough sink + decode-job batching" realistic?

**Partially, but the payoff is minimal.**

- **FrameSource:** Would only benefit add-locations (which already works fine). Sort uses random access. Merge's reader thread makes it incompatible with synchronous iteration.
- **Passthrough sink:** Savings ~30 lines at the cost of making logic harder to follow.
- **Decode-job batching:** The batch *processing* differs fundamentally between commands.

### Would unification help or hurt performance?

**Risk of regression is real, benefit is near-zero:**

- **Merge** runs at 11.9s for 18.8 GB (North America, uring+none). Every layer of indirection matters.
- **Add-locations** would lose its two-phase read optimization or require the pipeline to support it as a special case.
- **Cat's passthrough** is too simple to benefit.

### Minimum viable unification

1. Move `CopyRange` from add-locations to `mod.rs` (limited value since merge uses per-blob copy for interleaving reasons).
2. Unify `coalesce_passthrough` with `Option<&BlobIndex>` parameter (~10 lines saved).
3. Extract header-reading loop pattern (~15 lines saved).

## Recommendation: DO NOT DO

**What's already shared is the right level of abstraction.** `RawBlobFrame`, `read_raw_frame`, `flush_passthrough_buf`, `flush_block`, `flush_local`, `drain_batch_results`, `for_each_primitive_block_batch`, batch sizing constants capture all genuine commonality.

**The orchestration logic is fundamentally different per command.** Abstracting these into a "pipeline" would produce callback soup harder to understand than the current code.

**Performance risk is asymmetric.** Upside at best -2%. Downside +3% to +12%. For merge at planet scale (~47s), a 10% regression is 4.7 seconds -- unacceptable for a cosmetic improvement.
