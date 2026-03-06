# Consolidation Analysis (2026-03-07)

Post-refactor review of remaining duplication and consolidation opportunities across `src/commands/`.

## Current State: What's Already Well-Factored

The codebase has been significantly cleaned up since the March 3rd review. Key shared infrastructure in `mod.rs`:

- **Read pipeline**: `read_raw_frame`, `RawBlobFrame`, `read_blob_header_only`
- **Write pipeline**: `writer_from_header`, `writer_from_header_bytes`, `HeaderOverrides`
- **Batching**: `for_each_primitive_block_batch`, `drain_batch_results` (constants: BATCH_SIZE=64, BATCH_BYTE_BUDGET=128MB, BATCH_MIN/MAX_BLOBS=8/128)
- **Block flush**: `flush_block` (sequential), `flush_local` (rayon), `ensure_*_capacity` / `ensure_*_capacity_local` (6 variants)
- **Metadata**: `element_metadata`, `dense_node_metadata`, `element_raw_metadata`, `dense_node_raw_metadata`, `clean_metadata`
- **ID sets**: `IdSetDense` (shared chunked sparse bitset) used by extract, tags_filter, add_locations_to_ways
- **Streaming cursors**: `StreamingBlocks` + `fill_buffer` + `next_element` in `stream_merge.rs`, shared by diff and derive_changes

All commands writing PBF output follow one of two clean patterns:
1. **Rayon parallel**: `into_blocks_pipelined()` -> `for_each_primitive_block_batch` -> `map_init(BlockBuilder::new, ...)` -> `drain_batch_results`
2. **Sequential main-thread**: `ensure_*_capacity()` -> `flush_block()` -> direct writer

## Remaining Consolidation Opportunities

### 1) Unify Owned Element Types

**Status: 4 independent copies exist**

| Location | Used By | Has Ord? | Has Enum? |
|---|---|---|---|
| `owned_elements.rs` | diff, derive_changes | No (equality fns only) | No |
| `sort.rs:77-139` | sort sweep merge | Yes (BinaryHeap) | No |
| `merge_pbf.rs:88-150` | merge_pbf dedup sweep | Yes (BinaryHeap) | No |
| `time_filter.rs:48-101` | time_filter snapshot | No | Yes (`OwnedElement` enum) |

**Root cause**: sort and merge_pbf need `Ord` for BinaryHeap; shared version only provides equality functions.

**Refactor**: Add `Ord` impls (compare by ID only) to the shared `owned_elements.rs`. Sort and merge_pbf can then import instead of maintaining local copies. time_filter's `OwnedElement` enum is structurally different (dispatch wrapper) but could use the shared inner types.

**Risk**: Low. Behavioral — sort/merge_pbf compare by ID only, which is trivial to verify.

**Impact**: Eliminates ~180 lines of duplicated struct definitions. Prevents drift between copies.

### 2) Shared Merge-Join Function for diff/derive_changes

**Status: Two independent two-pointer merge-joins with different trait abstractions**

- `diff.rs`: `DiffElement` trait (6 methods) + `streaming_diff_phase()` — emits immediately per pair
- `derive_changes.rs`: `MergeElement` trait (4 methods) + `streaming_merge_phase()` — collects into Vecs

Both follow the same two-pointer pattern: advance whichever cursor has the smaller ID, classify as create/delete/modify/equal.

**Refactor**: Extract a generic `merge_join_sorted()` that takes two element streams and an action callback:
```
fn merge_join_sorted<T>(
    old: &mut impl FnMut() -> Option<T>,
    new: &mut impl FnMut() -> Option<T>,
    on_old_only: impl FnMut(T),      // delete
    on_new_only: impl FnMut(T),      // create
    on_both: impl FnMut(T, T),       // match (caller checks equality)
)
```
diff emits inside the callbacks; derive_changes collects into Vecs.

**Risk**: Low. Both already share `StreamingBlocks` cursor infrastructure. Behavioral equivalence easy to verify via existing snapshot tests.

**Impact**: Eliminates ~80 lines of duplicated control flow. Makes the merge-join contract explicit. Prepares for future streaming merge-join (P3-22).

### 3) Extract Reader Thread to Shared Helper

**Status: merge.rs spawns its own bounded-channel reader thread (lines 832-863)**

Pattern: spawn thread -> `read_raw_frame` loop -> send `RawBlobFrame` over mpsc channel -> skip OsmHeader.

This is currently merge-specific, but the same pattern would benefit sort (which does sequential seeks) and add_locations_to_ways (which has its own scan loop). If more commands adopt raw-frame streaming, this should be extracted.

**Refactor**: `spawn_reader_thread(path, direct_io) -> (JoinHandle, Receiver<RawBlobFrame>)` in mod.rs.

**Risk**: Low. Pure extraction, no behavioral change for merge.

**Impact**: Small (~30 lines), but prevents re-invention when new commands need the pattern. Not urgent unless a second consumer appears.

### 4) Normalize I/O Option Fields

**Status: 3 option structs share identical fields**

```
SortOptions:     compression, direct_io, io_uring, force
MergePbfOptions: compression, direct_io, io_uring, force
MergeOptions:    compression, direct_io, io_uring, force, locations_on_ways
```

**Refactor**: Extract `IoOptions { compression, direct_io, io_uring, force }` and embed it.

**Risk**: Negligible. Internal-only struct, no API surface.

**Impact**: Small (~20 lines saved). Prevents defaults from drifting between commands. Low priority.

## What Does NOT Need Consolidation

These were identified in the March 3rd review but are now resolved or unnecessary:

1. **Rewrite engine / ElementEmitter**: The `flush_block`/`flush_local`/`ensure_*_capacity` helpers plus `drain_batch_results` already provide the shared rewrite scaffolding. The remaining per-command code is genuinely command-specific (filter predicates, spatial checks, coordinate injection). A higher-level engine would add abstraction without reducing real duplication.

2. **Blob pipeline unification**: The raw-frame passthrough paths in merge, sort, cat, and add_locations_to_ways look similar but have fundamentally different classification policies (merge: diff-range overlap, sort: blob permutation with overlap sweeps, cat: type filtering, ALTW: node passthrough + way decode). Unifying these behind a generic framework would add branchy dispatch in the hottest paths for marginal code savings.

3. **Dependency-closure planner**: extract, tags_filter, and getid all do multi-pass ID collection, but their expansion rules differ enough that a generic planner would be as complex as the individual implementations. IdSetDense is already shared — the actual closure logic is command-specific.

4. **getid's BTreeSet vs IdSetDense**: getid uses BTreeSet for small CLI-specified ID lists. This is correct — IdSetDense would waste memory for <1000 IDs.

## Suggested Sequence

1. **Owned element unification** — lowest risk, cleanest win, no perf implications
2. **Shared merge-join** — small, well-scoped, prepares for streaming merge-join work
3. **I/O option normalization** — trivial, do opportunistically
4. **Reader thread extraction** — defer until a second consumer needs it

## Summary

The March refactor addressed the largest consolidation gaps (batch processing, block flushing, metadata helpers, ID sets, streaming cursors). What remains is smaller-scale: 4 copies of owned element types, 2 copies of merge-join logic, and 3 copies of I/O option fields. The recommended work is ~300 lines of net reduction with low regression risk.
