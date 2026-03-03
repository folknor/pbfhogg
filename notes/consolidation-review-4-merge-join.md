# Consolidation Review #4: Generic Merge-Join Core

## Verdict: ALREADY DONE

The streaming rewrite has addressed the core concern raised by this consolidation item.

## What the proposal identified as problems

1. `diff` and `derive_changes` both load owned vectors, sort by ID, and run per-type merge-join loops with near-identical control flow.
2. `sort` defines another owned element family.
3. The proposal called for a generic `merge_join_by_id(old, new, on_delete, on_create, on_match)` utility shared across both commands, plus aligning on a single owned domain model.

## What was actually accomplished

### 1. Shared streaming infrastructure (`stream_merge.rs`)

Both `diff` and `derive_changes` now import and use the shared `StreamingBlocks` cursor and its supporting functions from `src/commands/stream_merge.rs`:

- `StreamingBlocks` -- block-level cursor with stashing (lines 22-42)
- `next_element<T>()` -- generic element-at-a-time pull function (lines 119-133)
- `fill_buffer<T>()` -- block-level buffering with type-phase detection (lines 58-113)
- Block type predicates: `is_node_block`, `is_way_block`, `is_relation_block` (lines 139-149)
- Conversion functions: `convert_node`, `convert_way`, `convert_relation` (lines 155-203)

### 2. Shared owned element types (`owned_elements.rs`)

Both commands share the same owned element family:
- `OwnedNode`, `OwnedWay`, `OwnedRelation`, `OwnedMember`
- Shared equality functions: `nodes_equal`, `ways_equal`, `relations_equal`, `members_equal`
- Shared coordinate utilities: `from_decimicro`, `format_coord`

### 3. `OwnedMember` consolidation with sort

Sort's `OwnedRelation` uses `OwnedMember` imported from `owned_elements.rs` (line 27: `use super::owned_elements::OwnedMember`).

## What remains separate (intentionally)

**Sort's owned element types are kept distinct.** Sort defines its own `OwnedNode`, `OwnedWay`, and `OwnedRelation` in `sort.rs`. These are structurally different:

- Sort's types carry full `OwnedMetadata` (version, timestamp, changeset, uid, user, visible) needed for lossless roundtripping.
- Sort's types implement `Ord`/`PartialOrd`/`Eq` for use in a `BinaryHeap` sweep merge.
- The diff/derive_changes types only carry `version: Option<i32>`.

This separation is correct and intentional.

**The merge-join loop itself is not a single shared function.** Instead:

- `diff.rs` defines a `DiffElement` trait and `streaming_diff_phase<T: DiffElement>()` that handles output formatting.
- `derive_changes.rs` defines a `MergeElement` trait, a `MergeAction` enum, `classify()`, and `streaming_merge_phase<T: MergeElement>()` that collects elements into Vecs for XML serialization.

These have the same abstract shape (two-pointer walk) but genuinely different per-step actions.

## Remaining duplication (minor)

The `DiffElement` trait and `MergeElement` trait are nearly identical:

| `DiffElement` | `MergeElement` |
|---|---|
| `fn id(&self) -> i64` | `fn id(&self) -> i64` |
| `fn version(&self) -> Option<i32>` | (not needed) |
| `fn type_char() -> char` | (not needed) |
| `fn is_block_type(bt) -> bool` | `fn is_block_type(bt) -> bool` |
| `fn equal(&self, other: &Self) -> bool` | `fn equal(a: &Self, b: &Self) -> bool` |
| `fn convert(element) -> Option<Self>` | `fn convert(element) -> Option<Self>` |

A unified trait could exist but diff-specific methods would be dead weight in derive_changes, and the signature difference on `equal` reflects different ergonomic preferences. The impl blocks are ~6 lines each and delegate to shared functions.

## Conclusion

**This consolidation item is done.** The high-value work has been completed:

1. Streaming cursor infrastructure is fully shared via `stream_merge.rs`.
2. Owned element types are shared via `owned_elements.rs`.
3. `OwnedMember` is consolidated between sort and diff/derive_changes.
4. Both commands now operate in constant memory via streaming merge-join.

The remaining per-command merge-join loops (~30 lines each) and trait definitions (~20 lines each) are not worth further consolidation. The callback-based `merge_join_by_id()` would save ~40-50 lines at the cost of a more complex generic API. The current design is clearer and more maintainable.
