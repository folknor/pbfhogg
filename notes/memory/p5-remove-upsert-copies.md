# P5: Remove Per-Job Inline Upsert Copies

Action plan for replacing owned per-job `Vec<i64>` upsert copies with shared-range views into the sorted upsert arrays already maintained by `DiffRanges`.

## Current State

### Where `to_vec()` happens

File: `src/commands/merge.rs`, line 1118, inside the Phase 2 sequential inline-assign loop:

```rust
ClassifyResult::NeedsRewrite(block, index) => {
    let upserts = ranges.upserts(index.kind);
    let start = upserts.partition_point(|&id| id < index.min_id);
    let end = upserts[start..].partition_point(|&id| id <= index.max_id) + start;
    let inline_upserts = upserts[start..end].to_vec();  // <-- THE COPY

    let job_idx = rewrite_jobs.len();
    rewrite_jobs.push(RewriteJob {
        block,
        kind: index.kind,
        inline_upserts,   // owned Vec<i64>
    });
    slots.push(BatchSlot::Rewrite { job_index: job_idx, index });
}
```

### What `RewriteJob` looks like

```rust
struct RewriteJob {
    block: PrimitiveBlock,
    kind: ElemKind,
    inline_upserts: Vec<i64>,  // owned copy
}
```

### What data is copied

Each `to_vec()` copies a contiguous subslice of sorted `i64` IDs. The slice `upserts[start..end]` contains the create/modify IDs that fall within `[min_id, max_id]` for one blob.

### Estimated bytes per copy

- A typical daily diff for planet has ~200K-700K changes. For Germany (146K changes, 18.4% rewrite ratio), rewritten blobs number ~850 out of ~4,600 total. For planet (~92% rewrite ratio, ~2.5M blobs), ~2.3M blobs would be rewritten.
- Each rewrite blob gets a subset of the diff's upsert IDs. With ~200K upserts spread across ~2.3M rewritten blobs, most subsets are tiny (0-5 IDs). Each `i64` is 8 bytes; per-blob Vec overhead is 24 bytes (ptr+len+cap).
- **Per-blob cost:** 24 bytes (Vec header) + 8 * N bytes (IDs) + allocator overhead (~16-32 bytes for small allocs).
- **Aggregate at planet scale (2.3M rewritten blobs):** ~2.3M Vec allocations, each tiny. Total data: ~200K * 8 = 1.6 MB of IDs copied. Total allocator overhead: ~2.3M * ~48 bytes = ~110 MB of allocator metadata/fragmentation pressure.
- The memory cost is not in the ID bytes themselves, but in the **allocation count** (~2.3M small heap allocations per merge) and the associated allocator churn, fragmentation, and cache pollution. These allocations happen on the main thread during Phase 2 and are freed by rayon workers during Phase 3, creating cross-thread deallocation patterns that stress the allocator.

## Full Data Flow

### 1. Overlay construction (pre-merge)

`parse_osc_file()` in `src/osc.rs` parses an `.osc.gz` into `DiffOverlay`:
- `DiffOverlay.nodes: HashMap<i64, OscNode>` (create/modify nodes)
- `DiffOverlay.ways: HashMap<i64, OscWay>` (create/modify ways)
- `DiffOverlay.relations: HashMap<i64, OscRelation>` (create/modify relations)
- `DiffOverlay.deleted_{nodes,ways,relations}: HashSet<i64>` (deleted IDs)

Multiple diffs can be loaded via `load_all_diffs()` and merged with `DiffOverlay::merge()`.

### 2. DiffRanges construction (merge lines 961-966)

`DiffRanges::from_diff(&diff)` extracts and sorts ID vectors:
- `node_ids`, `way_ids`, `rel_ids` -- all affected IDs (upserts + deletes), sorted. Used for coarse range overlap checks.
- `node_upserts`, `way_upserts`, `rel_upserts` -- create/modify IDs only (no deletes), sorted. Used for inline assignment and gap create tracking.

These six `Vec<i64>` are allocated once and live for the entire merge. They are **read-only** after construction.

### 3. Batch processing loop

For each batch of up to 64 raw blob frames:

**Phase 1 (parallel classify):** `classify_only()` runs in rayon, reads `DiffRanges` (shared `&`) and `DiffOverlay` (shared `&`). Returns `ClassifyResult::NeedsRewrite(block, index)` for affected blobs.

**Phase 2 (sequential inline assign):** The main thread iterates classify results. For each `NeedsRewrite`:
- Calls `ranges.upserts(index.kind)` to get the **shared sorted array** for this element type (e.g., `&[i64]` backed by `DiffRanges.way_upserts`).
- Uses `partition_point` twice to find `start..end` within the sorted array -- the IDs in `[min_id, max_id]`.
- **Copies** `upserts[start..end].to_vec()` into `RewriteJob.inline_upserts`.
- Pushes `RewriteJob { block, kind, inline_upserts }` into `rewrite_jobs: Vec<RewriteJob>`.

**Phase 3 (parallel rewrite):** `rewrite_jobs.par_iter().map_init(...)` runs `rewrite_block_parallel()` for each job. Each invocation reads:
- `&job.inline_upserts` -- the owned Vec from Phase 2 (this is what we want to eliminate).
- `&diff` -- shared reference to `DiffOverlay` for entity lookups.
- `&job.block` -- the parsed `PrimitiveBlock`.

Inside `rewrite_block_parallel()` (line 654), `inline_upserts` is accessed as `&[i64]`:
- A `upsert_cursor` walks forward through the slice.
- Before each base element, creates with IDs < current element ID are emitted.
- Modification IDs equal to current element ID are skipped (handled by diff lookup).
- After all base elements, remaining trailing creates are emitted.

The function signature is:
```rust
fn rewrite_block_parallel(
    block: &PrimitiveBlock,
    diff: &DiffOverlay,
    bb: &mut BlockBuilder,
    inline_upserts: &[i64],  // <-- already a slice reference
    kind: ElemKind,
) -> MergeResult<RewriteOutput>
```

**Phase 4 (sequential output):** Rewrite outputs are drained. The `inline_upserts` Vec is implicitly dropped when `rewrite_jobs` goes out of scope at the end of the batch loop iteration.

### 4. Key observation

`rewrite_block_parallel` already takes `&[i64]`, not `Vec<i64>`. It only needs a borrowed slice. The `Vec<i64>` in `RewriteJob` exists solely to keep the data alive for the duration of Phase 3. But the data is already alive -- it lives in `DiffRanges.{node,way,rel}_upserts`, which is stack-owned in `merge()` and outlives all batch iterations.

## Proposed Design: Range Views into Shared Sorted Arrays

### Core change

Replace the owned `Vec<i64>` in `RewriteJob` with a `(usize, usize)` range pair that indexes into the already-existing `DiffRanges` upsert arrays.

#### New `RewriteJob` definition

```rust
struct RewriteJob {
    block: PrimitiveBlock,
    kind: ElemKind,
    upsert_range: (usize, usize),  // (start, end) into DiffRanges.upserts(kind)
}
```

#### Phase 2 change (inline assign)

```rust
ClassifyResult::NeedsRewrite(block, index) => {
    let upserts = ranges.upserts(index.kind);
    let start = upserts.partition_point(|&id| id < index.min_id);
    let end = upserts[start..].partition_point(|&id| id <= index.max_id) + start;
    // No to_vec() -- just record the range
    let job_idx = rewrite_jobs.len();
    rewrite_jobs.push(RewriteJob {
        block,
        kind: index.kind,
        upsert_range: (start, end),
    });
    slots.push(BatchSlot::Rewrite { job_index: job_idx, index });
}
```

#### Phase 3 change (parallel rewrite)

```rust
let rewrite_results: Vec<Result<RewriteOutput, String>> = rewrite_jobs
    .par_iter()
    .map_init(
        BlockBuilder::new,
        |thread_bb, job| {
            let upserts = ranges.upserts(job.kind);
            let inline_slice = &upserts[job.upsert_range.0..job.upsert_range.1];
            rewrite_block_parallel(
                &job.block,
                &diff,
                thread_bb,
                inline_slice,
                job.kind,
            )
            .map_err(|e| e.to_string())
        },
    )
    .collect();
```

### Lifetime and thread safety

**Why this works without `Arc` or any synchronization:**

1. `DiffRanges` (`ranges`) is a local variable on the stack of `merge()`. It lives from line 962 to the end of `merge()` (line 1311). All batch iterations happen within this scope.

2. `ranges` is already shared with `par_iter()` closures in Phase 1 (`classify_only`). The same borrow pattern works for Phase 3.

3. rayon's `par_iter().map_init()` borrows the closure's captures by shared reference. Since `ranges` is `&DiffRanges` and `DiffRanges` fields are `Vec<i64>` (which implements `Sync`), the `&[i64]` slice obtained from `ranges.upserts(kind)` is safely shareable across rayon worker threads.

4. No `Arc` is needed. No scoped threads are needed. The existing borrow-from-stack pattern used for `&diff` and `&ranges` in Phase 1 already establishes the lifetime precedent.

**The `rewrite_block_parallel` function signature does not change.** It already takes `&[i64]`. The only difference is where the backing storage lives: previously a per-job `Vec<i64>` on the heap, now a subslice of `DiffRanges`'s `Vec<i64>` on the stack (which was already on the heap as the Vec's backing buffer, but shared rather than copied).

### Data structure summary

Before:
```
DiffRanges.node_upserts: Vec<i64>  [shared, read-only]
DiffRanges.way_upserts:  Vec<i64>  [shared, read-only]
DiffRanges.rel_upserts:  Vec<i64>  [shared, read-only]

RewriteJob {
    inline_upserts: Vec<i64>,  // COPY of a subslice from above
}
```

After:
```
DiffRanges.node_upserts: Vec<i64>  [shared, read-only]
DiffRanges.way_upserts:  Vec<i64>  [shared, read-only]
DiffRanges.rel_upserts:  Vec<i64>  [shared, read-only]

RewriteJob {
    upsert_range: (usize, usize),  // indexes into the above
}
```

## Interaction with P1 (Compact Diff Model)

P1 proposes replacing `DiffOverlay`'s `HashMap<i64, OscNode/Way/Relation>` with arena-based storage and `id -> offset` indexes. This does **not** affect P5 because:

1. P5 operates on `DiffRanges`, which is a **derived** data structure built from `DiffOverlay` at merge start. `DiffRanges` contains only sorted `Vec<i64>` arrays -- it does not store entity payloads.

2. Even if P1 redesigns `DiffOverlay` internals, `DiffRanges::from_diff()` only calls `.keys()` and `.iter()` on the overlay's collections to extract IDs. Whether those IDs come from a `HashMap`, a sorted arena index, or a `BTreeMap` does not matter -- `DiffRanges` just needs an iterator of `i64` IDs.

3. If P1 makes the overlay itself sorted (e.g., arena with sorted ID index), `DiffRanges` could potentially be eliminated entirely, since the overlay's own sorted index could serve the same purpose. In that case, `upsert_range` would index into the overlay's sorted ID array instead. The range-view pattern adapts trivially.

**Recommended order:** P5 before P1. P5 is a small, contained change that reduces allocation churn immediately. P1 is a larger redesign. If P1 later changes the data structures, the `upsert_range` pattern carries forward with a one-line change to the slice source.

## Interaction with P3 (Streaming Rewrite Outputs)

P3 proposes streaming rewrite outputs incrementally to the writer instead of collecting all `RewriteOutput`s before Phase 4. This does **not** affect P5 because:

1. P5 changes what goes **into** the rewrite function (slice reference vs owned Vec). P3 changes what comes **out** (incremental streaming vs collected vector).

2. The shared sorted array (`DiffRanges`) must live at least as long as Phase 3 completes. With the current design, `DiffRanges` lives until `merge()` returns. Even if P3 makes Phase 3 and Phase 4 interleave or overlap, `DiffRanges` still outlives both.

3. One subtle consideration: if P3 changes the batch loop so that rewrite jobs from multiple batches are in flight simultaneously (pipelining batches), the `DiffRanges` borrow still works because it is created once and lives for the entire merge. The `upsert_range` indices are absolute positions in the sorted arrays, valid for the lifetime of `DiffRanges`.

**No design change needed for P3 compatibility.** The shared sorted array is inherently compatible with streaming output because the array's lifetime (`merge()` scope) is the maximum possible.

## Memory Savings Estimate

### Current: per-job Vec allocations

| Component | Per-blob | Planet-scale (2.3M rewrite blobs) |
|---|---|---|
| Vec header (ptr+len+cap) | 24 bytes | 55 MB |
| ID data (avg ~0.1 IDs/blob * 8 bytes) | ~1 byte | ~2.3 MB |
| Allocator overhead (~32 bytes/alloc) | 32 bytes | 74 MB |
| **Total** | **~57 bytes** | **~131 MB** |

Note: the ID data itself is small because daily diff upserts (~200K-700K) are spread thinly across ~2.3M rewritten blobs. Most blobs get 0-2 upsert IDs. The cost is dominated by allocation count, not data volume.

For weekly backlog merges (S5 scenario with 7-30 diffs coalesced, ~1-5M upserts), the per-blob upsert counts increase, but the allocation count stays proportional to rewritten blob count.

### Proposed: range pairs

| Component | Per-blob | Planet-scale (2.3M rewrite blobs) |
|---|---|---|
| `(usize, usize)` | 16 bytes | 37 MB |
| Allocator overhead | 0 bytes (inline in RewriteJob, no heap alloc) | 0 |
| **Total** | **16 bytes** | **37 MB** |

### Net savings

- **Eliminated:** ~2.3M heap allocations per merge (the primary benefit -- reduced allocator churn, reduced cross-thread deallocation, reduced fragmentation).
- **RSS reduction:** ~94 MB at planet scale. Modest but guaranteed.
- **Throughput gain:** small but nonzero. Eliminates `to_vec()` memcpy (though the data is tiny) and avoids allocator contention from 2.3M small allocations flowing from main thread to rayon workers.

The real value is not RSS savings but **allocation churn elimination**: 2.3M fewer `malloc`/`free` cycles per merge, with the cross-thread deallocation pattern (allocated on main thread, freed on rayon workers) that is known to cause allocator lock contention.

## Implementation Steps

### Step 1: Change `RewriteJob` struct

In `src/commands/merge.rs`, replace:

```rust
struct RewriteJob {
    block: PrimitiveBlock,
    kind: ElemKind,
    inline_upserts: Vec<i64>,
}
```

With:

```rust
struct RewriteJob {
    block: PrimitiveBlock,
    kind: ElemKind,
    upsert_range: (usize, usize),
}
```

### Step 2: Update Phase 2 (inline assign)

Replace (around line 1114-1125):

```rust
let upserts = ranges.upserts(index.kind);
let start = upserts.partition_point(|&id| id < index.min_id);
let end = upserts[start..].partition_point(|&id| id <= index.max_id) + start;
let inline_upserts = upserts[start..end].to_vec();

let job_idx = rewrite_jobs.len();
rewrite_jobs.push(RewriteJob {
    block,
    kind: index.kind,
    inline_upserts,
});
```

With:

```rust
let upserts = ranges.upserts(index.kind);
let start = upserts.partition_point(|&id| id < index.min_id);
let end = upserts[start..].partition_point(|&id| id <= index.max_id) + start;

let job_idx = rewrite_jobs.len();
rewrite_jobs.push(RewriteJob {
    block,
    kind: index.kind,
    upsert_range: (start, end),
});
```

### Step 3: Update Phase 3 (parallel rewrite)

Replace (around line 1132-1147):

```rust
let rewrite_results: Vec<Result<RewriteOutput, String>> = rewrite_jobs
    .par_iter()
    .map_init(
        BlockBuilder::new,
        |thread_bb, job| {
            rewrite_block_parallel(
                &job.block,
                &diff,
                thread_bb,
                &job.inline_upserts,
                job.kind,
            )
            .map_err(|e| e.to_string())
        },
    )
    .collect();
```

With:

```rust
let rewrite_results: Vec<Result<RewriteOutput, String>> = rewrite_jobs
    .par_iter()
    .map_init(
        BlockBuilder::new,
        |thread_bb, job| {
            let upserts = ranges.upserts(job.kind);
            let inline_slice = &upserts[job.upsert_range.0..job.upsert_range.1];
            rewrite_block_parallel(
                &job.block,
                &diff,
                thread_bb,
                inline_slice,
                job.kind,
            )
            .map_err(|e| e.to_string())
        },
    )
    .collect();
```

### Step 4: Verify compilation and tests

- `brokkr check` to run clippy + tests.
- `brokkr check -- --ignored` to run the full Denmark roundtrip.
- `brokkr verify merge` to cross-validate against osmium/osmosis/osmconvert.

### Step 5: Benchmark

- `brokkr bench merge --dataset denmark` for regression check.
- `brokkr bench merge --dataset germany` for rewrite-heavy scenario (18.4% rewrite ratio).
- If north-america data is available: `brokkr bench merge --dataset north-america` for large-scale validation.

### Total lines changed

This is a ~10-line diff. No new types, no new modules, no API changes. The `rewrite_block_parallel` function signature does not change.

## Risk Assessment

### Correctness of range computation

**Risk: LOW.** The range computation (`partition_point` calls) is identical before and after. The only change is removing the `.to_vec()` at the end. The resulting slice contains exactly the same data at exactly the same indices. Since `DiffRanges.{node,way,rel}_upserts` is never mutated after construction, the indices remain valid for the lifetime of `ranges`.

One edge case to verify: overlapping blob ranges. If two blobs in the same batch have overlapping `[min_id, max_id]` ranges, their `upsert_range` intervals may overlap (both include the same IDs). This is correct behavior -- the same upsert ID can appear in multiple blobs' ranges, and `rewrite_block_parallel` handles this by checking against actual base element IDs. This works identically whether the IDs come from a copied Vec or a shared slice.

### Lifetime complexity

**Risk: NONE.** There is zero additional lifetime complexity. The shared `&DiffRanges` reference is already used in Phase 1 (`classify_only`). Phase 3 adds the same borrow pattern. Rust's borrow checker enforces correctness at compile time. No `unsafe`, no `Arc`, no manual lifetime management.

### Thread safety

**Risk: NONE.** `&[i64]` is `Send + Sync`. rayon's `par_iter()` closure captures are bounded by the closure's lifetime. Since `ranges` outlives the `par_iter()` call (it lives on `merge()`'s stack), the borrow is valid. This is the same pattern already used for `&diff` in the same closure.

### Performance regression

**Risk: NONE.** We are strictly removing work (no `to_vec()`, no heap allocation). The `partition_point` calls happen regardless. The slice indexing in Phase 3 (`&upserts[range.0..range.1]`) is a single pointer+length computation, cheaper than the previous `&job.inline_upserts` (which was also a slice deref, but through a heap-allocated Vec).

### Interaction with future changes

**Risk: LOW.** The only assumption is that `DiffRanges` outlives all batch processing. This is structurally guaranteed by `merge()`'s stack layout. Any future refactoring that moves `DiffRanges` behind a reference or `Arc` preserves this property. If `DiffRanges` is eliminated in favor of P1's compact diff model, the `upsert_range` pattern trivially adapts to index into whatever sorted array replaces it.

## Summary

| Aspect | Value |
|---|---|
| **Complexity** | Trivial (~10-line diff) |
| **Risk** | Near-zero (compile-time safety, no behavioral change) |
| **Allocation savings** | ~2.3M fewer heap allocs per planet merge |
| **RSS savings** | ~94 MB (modest) |
| **Primary benefit** | Eliminates cross-thread alloc/dealloc churn |
| **Blocking dependencies** | None |
| **Blocked by** | Nothing |
| **Compatible with** | P1 (compact diff), P3 (streaming rewrite), all other P-items |
| **Recommended order** | Do before P1, can be done at any time |
