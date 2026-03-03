# Consolidation Review #1: Shared Command Rewrite Engine

## Verdict: DO NOT DO

## Pattern Inventory per Command

### A. cat.rs (314 lines, filtered path only)

**Pipeline setup:** `ElementReader::open` + `with_blob_filter` + `into_blocks_pipelined` (lines 238-239)
**Batching:** `for_each_primitive_block_batch(blocks_iter, BATCH_SIZE, ...)` (line 240)
**Parallel transform:** `batch.par_iter().map_init(BlockBuilder::new, |bb, block| { ... flush_local(bb, &mut output)?; Ok((output, count)) }).collect()` (lines 281-299)
**Drain:** `drain_batch_results(results, writer, |count| { ... })` (lines 304-307)
**Unique logic:** Type-filter booleans (filter_node/filter_way/filter_relation). Also has a completely separate passthrough path (lines 71-123) that bypasses the entire pattern.

### B. getid.rs (456 lines)

**Pipeline setup:** `ElementReader::open` + optional `with_blob_filter` + `into_blocks_pipelined` (lines 150-160)
**Batching:** `for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, ...)` (line 168)
**Parallel transform:** `batch.par_iter().map_init(BlockBuilder::new, |bb, block| { ... flush_local(bb, &mut output)?; Ok((output, (nodes, ways, relations))) }).collect()` (lines 353-366)
**Drain:** `drain_batch_results(results, writer, |(nodes, ways, relations)| { ... })` (lines 371-375)
**Unique logic:** ID-set membership test (include/exclude mode). Two-pass mode with `--add-referenced` collecting way node refs in pass 1 (lines 186-238). The pass 1 is sequential, NOT using the batch infrastructure.

### C. tags_filter.rs (866 lines, ~400 lines are expression parsing/tests)

**Pipeline setup:** `ElementReader::open` + optional `blob_filter_from_expressions` + `into_blocks_pipelined` (lines 370-374)
**Batching:** `for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, ...)` (line 383)
**Parallel transform:** `batch.par_iter().map_init(BlockBuilder::new, |bb, block| { ... flush_local(bb, &mut output)?; Ok((output, block_stats)) }).collect()` (lines 399-410)
**Drain:** `drain_batch_results(results, writer, |s| { ... })` (lines 412-417)
**Unique logic:** Tag expression matching against each element. Two-pass mode with `IdSetDense` for matched IDs + way dependencies (lines 550-643). Pass 2 has its own `process_pass2_batch` using the same par_iter/map_init/drain pattern but with `Pass2IdSets`.

### D. extract.rs (1650 lines, ~350 lines are geojson/bbox parsing/tests)

**Pipeline setup:** `ElementReader::open` + `with_blob_filter(spatial_blob_filter)` + `into_blocks_pipelined` (lines 675-676)
**Batching:** `for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, ...)` (lines 633, 755, 1095) -- used 3 times across simple/complete/smart
**Parallel transform:** `batch.par_iter().map_init(BlockBuilder::new, |bb, block| { ... flush_local(bb, &mut output)?; Ok((output, block_stats)) }).collect()` (lines 1014-1025, 1234-1245)
**Drain:** `drain_batch_results(results, writer, |s| merge_extract_stats(stats, &s))` (lines 1027, 1247)
**Unique logic:** Three separate strategies (simple, complete_ways, smart) each with their own multi-pass structure. Pass 1 is sequential classification with `IdSetDense`. Simple-single-pass does classify+write in one pass by manually batching (lines 683-714) instead of using `for_each_primitive_block_batch`. Three different `process_block` variants (`classify_block_simple`, `extract_block_pass2`, `extract_block_pass3`) with different `IdSets` structs.

### E. add_locations_to_ways.rs (961 lines)

**Pipeline setup (decode-all fallback):** `ElementReader::open` + `into_blocks_pipelined` (line 489)
**Batching (decode-all):** Manual batch loop with `batch.push(block?)` + `if batch.len() >= BATCH_SIZE` (lines 487-506) -- NOT using `for_each_primitive_block_batch`
**Parallel transform:** `batch.par_iter().map_init(|| (BlockBuilder::new(), Vec::<i64>::new(), Vec::<(i32, i32)>::new()), |(bb, refs_buf, locations_buf), block| { ... }).collect()` (lines 639-653)
**Drain:** `drain_batch_results(results, writer, |s| merge_stats(&mut total, &s))` (line 666)
**Unique logic:** The `map_init` takes a *3-tuple* `(BlockBuilder, Vec<i64>, Vec<(i32,i32)>)`, not just `BlockBuilder`. The passthrough path (lines 748-896) is a completely different architecture: two-phase read (header-only classification + selective data read/skip), `BatchSlot` enum, `DecompressPool` per worker, `CopyRange` for kernel-space copy, byte-budgeted batching with `BATCH_BYTE_BUDGET`/`BATCH_MIN_BLOBS`/`BATCH_MAX_BLOBS`.

### F. merge.rs (1552 lines)

**Pipeline setup:** Dedicated reader thread + `mpsc::sync_channel` + `collect_batch` (NOT `ElementReader` or `into_blocks_pipelined`) (lines 654-721)
**Batching:** Custom byte-budgeted `collect_batch` with `BATCH_BYTE_BUDGET`/`BATCH_MIN_BLOBS`/`BATCH_MAX_BLOBS` (lines 688-721)
**Parallel transform:** Phase 1: `batch.par_iter().map_init(Vec::new, |buf, frame| classify_only(...))` (lines 1177-1183). Phase 3: `rayon::spawn` with per-job `BlockBuilder` + `rewrite_block_parallel` (lines 1241-1256). NOT using `map_init` at all for the rewrite phase.
**Drain:** Streaming drain via `mpsc::sync_channel` with out-of-order buffering (lines 1259-1375) -- NOT using `drain_batch_results`.
**Unique logic:** Nearly everything. Raw frame reading, 4-phase pipeline (classify/assign/rewrite/output), `DiffRanges`, `UpsertCursors`, `CompactDiffOverlay`, gap creates, type transitions, passthrough coalescing, io_uring/copy_file_range, pre-seeded string tables with `add_*_raw_bytes`.

### G. sort.rs (814 lines)

**Pipeline:** No pipeline at all. Random-access `File::open` + `Seek` for blob-level permutation (lines 193-250)
**Batching:** No batching. Sequential single-blob passthrough or overlap-run decode
**Parallel transform:** None (sequential)
**Drain:** Direct `flush_block(bb, writer)` per element (lines 526, 560, 594)
**Unique logic:** Everything. Blob-level indexing, overlap detection, sweep merge with `BinaryHeap`, owned element types.

## The Actual Shared Infrastructure (Already Extracted)

The shared helpers in `mod.rs` (479 lines) already capture the truly generic parts:

- `for_each_primitive_block_batch` (lines 54-74) -- batch accumulation loop
- `drain_batch_results` (lines 271-284) -- sequential drain of parallel results
- `flush_local` (lines 290-298) -- thread-local block flush
- `flush_block` (lines 256-264) -- writer block flush
- `writer_from_header` (lines 317-326) -- header setup
- `element_metadata` / `dense_node_metadata` (lines 365-390) -- metadata extraction

## Assessment: How Similar Are the Patterns Really?

**The proposal claims:** "most transform commands implement the same high-level runtime: stream blocks -> optionally do ID-collection passes -> batch blocks -> run parallel per-block transform -> drain outputs in-order -> merge stats."

**What I actually found:**

The proposal is partially correct but significantly overstates the uniformity.

### Truly Shared (Already Factored Out)

The `process_batch` pattern that appears in cat, getid, tags_filter, and extract's write passes is:

```rust
type BatchResult = Result<(Vec<OwnedBlock>, S), String>;
let results: Vec<BatchResult> = batch
    .par_iter()
    .map_init(
        BlockBuilder::new,  // or a tuple with extra buffers
        |bb, block| {
            let mut output: Vec<OwnedBlock> = Vec::new();
            let stats = process_block(block, bb, &mut output, ...command-specific-args...)?;
            flush_local(bb, &mut output)?;
            Ok((output, stats))
        },
    )
    .collect();
drain_batch_results(results, writer, |s| merge_stats(...))?;
```

This 15-line skeleton appears ~8 times across the codebase. The existing `drain_batch_results` + `flush_local` + `for_each_primitive_block_batch` already factor out the 3 most generic pieces.

### What Differs Between Commands (The "Hooks")

1. **`map_init` initializer:** cat/getid/tags_filter/extract use `BlockBuilder::new`. add_locations_to_ways uses `(BlockBuilder::new(), Vec::<i64>::new(), Vec::<(i32,i32)>::new())`. merge uses `Vec::new()` for classify and per-task `BlockBuilder::new()` for rewrite. These are not unifiable without boxing or an associated type.

2. **`process_block` signature:** Every command has a different signature:
   - cat: `(block, bb, output, filter_node, filter_way, filter_relation) -> u64`
   - getid: `(block, bb, output, ids, include, dep_node_ids) -> (u64, u64, u64)`
   - tags_filter: `(block, expressions, bb, output) -> TagsFilterStats`
   - extract pass2: `(block, ids, bb, output) -> ExtractStats`
   - extract pass3: `(block, ids, bb, output) -> ExtractStats`
   - add_locations_to_ways: `(block, bb, output, index, keep_untagged_nodes, refs_buf, locations_buf) -> Stats`

3. **Stats type:** Each command has its own stats struct. The `drain_batch_results` already handles this via the generic `S` type parameter.

4. **The element-level match arms:** The core per-element code is 15-30 lines per element type per command, and it's genuinely different for each command.

### Commands That Don't Fit the Pattern At All

- **merge.rs:** Uses a completely different architecture.
- **sort.rs:** No pipelining, no batching, no parallel transform.
- **add_locations_to_ways passthrough path:** Two-phase read, closer to merge's architecture.

## What Would a "Shared Rewrite Engine" Look Like?

### Attempt at a Unified Trait

```rust
trait RewriteCommand {
    type Context: Send + Sync;
    type BlockState: Send;
    type Stats: Send;

    fn setup_reader(&self, input: &Path) -> Result<ElementReader>;
    fn collect_pass(&self, blocks: &[PrimitiveBlock]) -> Self::Context;
    fn init_block_state(&self) -> Self::BlockState;
    fn process_block(
        &self,
        block: &PrimitiveBlock,
        ctx: &Self::Context,
        bb: &mut BlockBuilder,
        state: &mut Self::BlockState,
        output: &mut Vec<OwnedBlock>,
    ) -> Result<Self::Stats, String>;
    fn merge_stats(&self, target: &mut Self::Stats, source: Self::Stats);
}
```

### Problems With This Approach

1. **Lifetime issues with `Self::Context`:** For getid, the context contains `&IdSet` (borrowed from function args). For tags_filter, it's `&[Expression]`. A trait can't express "the context borrows from the function's stack frame" without GATs or `'ctx` lifetime parameters that infect everything.

2. **Multiple passes don't compose:** extract has 1-3 passes depending on strategy. A single `collect_pass` hook doesn't cover the diversity.

3. **The `map_init` initializer varies:** add_locations_to_ways needs extra buffers in the initializer tuple.

4. **Two completely different "rewrite" architectures coexist:** The ElementReader-based decode+rebuild path and the raw-frame passthrough+selective-rewrite path.

## Quantitative Analysis

**Lines that follow the batch pattern:**

| Command | Instances | Lines per instance | Total |
|---------|-----------|-------------------|-------|
| cat.rs | 1 | ~20 | 20 |
| getid.rs | 1 | ~25 | 25 |
| tags_filter.rs | 2 | ~25 each | 50 |
| extract.rs | 2 | ~15 each | 30 |
| add_locations_to_ways.rs | 2 | ~25 each | 50 |

**Total "duplicated" skeleton code: ~175 lines across 8 instances.**

The actual boilerplate that's identical across all 8 instances is ~8 lines per instance, totaling **~64 lines**.

**Unique logic per command:**

| Command | Unique logic lines |
|---------|-------------------|
| cat.rs | ~80 |
| getid.rs | ~120 |
| tags_filter.rs | ~200 |
| extract.rs | ~600 |
| add_locations_to_ways.rs | ~500 |
| merge.rs | ~900 |
| sort.rs | ~600 |

## Risk Assessment

**Performance regression risk: MODERATE TO HIGH.**

The current code is carefully tuned for each command's access patterns:
- merge uses `pre_seed_string_table` + `add_*_raw_bytes` to avoid string table rebuilds
- add_locations_to_ways uses extra `refs_buf`/`locations_buf` in the initializer
- extract's single-pass mode interleaves classify+write

Adding a trait indirection layer risks:
- Extra virtual dispatch overhead in hot loops
- Loss of inlining opportunities
- Increased cognitive complexity

**Effort estimate: 10-14 days of focused effort**

## Recommendation: DO NOT DO THIS

**The duplication is overstated.** The 64 lines of true batch-skeleton boilerplate across 8 instances is well within the range of acceptable copy-paste. The existing shared infrastructure in `mod.rs` already captures the genuinely reusable parts.

**The abstraction would be forced.** The commands differ in their `process_block` signatures, stats types, init state, and multi-pass structures.

**The "different" commands are the important ones.** merge and add_locations_to_ways are the performance-critical production paths.

**A smaller version is also not worth it.** The resulting function signature would be complex enough that calling it wouldn't be meaningfully simpler than the current 8-line inline pattern.

**The current approach is correct for this codebase.** Each command is self-contained, readable, and independently optimizable. The ~64 lines of boilerplate are a small price for the clarity and performance control this provides.
