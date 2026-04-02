# Block-pair merge-join: implementation plan

Prerequisite: [fill-buffer-optimization.md](fill-buffer-optimization.md)
for the problem analysis and approach comparison.

## Overview

Replace the element-level `fill_buffer` → `next_element` → `merge_join_phase`
architecture with a block-pair merge-join that operates at blob level first,
falling through to element level only for overlapping blocks.

**Impact:** 80.7 GB → ~1 GB cumulative alloc (Japan diff), 98.8% of elements
skip decode entirely.

## Current architecture

```
StreamingBlocks → fill_buffer(convert) → Vec<OwnedT> → next_element → merge_join_phase
```

Two `StreamingBlocks` cursors yield owned elements one at a time. Every element
is materialized with String allocations for tags. `merge_join_phase` compares
element IDs and calls `T::equal()` for matching IDs.

## Proposed architecture

```
BlobCursor → blob_index comparison → {skip | decode_and_element_merge}
```

### Phase 1: Blob-level indexdata comparison

Each sorted PBF blob has `BlobIndex { kind, min_id, max_id, count, bbox }`.
For two sorted PBF files with identical block boundaries (common for
files from the same source), blob-level comparison is:

```
old_blob.index.min_id == new_blob.index.min_id &&
old_blob.index.max_id == new_blob.index.max_id &&
old_blob.index.count == new_blob.index.count
```

If the ID ranges and counts match, the blobs *might* be identical.
We can then compare the compressed bytes for definitive equality.

### Phase 2: Compressed byte comparison

If `old_blob.compressed_data == new_blob.compressed_data`, all elements
are identical. Emit count as `Equal`, skip decode.

This is sound because:
- Same compressed bytes → same decompressed bytes → same elements
- zlib/zstd are deterministic (same input → same output)
- The string table is block-scoped (same bytes = same string table)

### Phase 3: Non-matching blocks → element-level fallback

When blobs differ, fall through to the current element-level merge-join.
This only happens for the ~1.2% of blobs that actually changed.

## Implementation steps

### Step 1: BlobCursor — blob-level access without decode

New struct in `stream_merge.rs`:

```rust
pub(crate) struct BlobCursor {
    blob_reader: BlobReader<FileReader>,
    decompress_pool: DecompressPool,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
}

impl BlobCursor {
    fn new(path: &Path, direct_io: bool) -> Result<Self>;

    /// Peek at the next blob's index without decompressing.
    /// Returns None at EOF or when the next blob's type doesn't match.
    fn peek_index(&mut self) -> Result<Option<&BlobIndex>>;

    /// Skip the current blob (advance past it without decoding).
    fn skip_blob(&mut self) -> Result<()>;

    /// Decode the current blob into a PrimitiveBlock.
    fn decode_blob(&mut self) -> Result<PrimitiveBlock>;

    /// Get the compressed data bytes for the current blob (for equality check).
    fn compressed_bytes(&self) -> Option<&[u8]>;

    /// Get the element count from indexdata (for Equal stats).
    fn element_count(&self) -> u64;
}
```

**Challenge:** `BlobReader` currently consumes each blob when iterated.
We need to hold the blob for inspection before deciding whether to decode.
The `Blob` struct already has `index()` and access to `WireBlob.data`.

**Solution:** Don't use the iterator interface. Use `BlobReader`'s
internal methods to read the header first, check the index, then
either skip the data or read and decode it.

Actually, `BlobReader` already has `set_parse_indexdata(true)` and
the `Blob` struct exposes `index()`. The `Blob` also holds `WireBlob`
which has `data: Option<BlobData>` with the compressed bytes. We can
access `blob.blob.data` to get the compressed bytes for comparison.

Simpler approach: iterate `BlobReader` normally, but before decompressing,
check the index and optionally compare compressed bytes:

```rust
for blob_result in &mut blob_reader {
    let blob = blob_result?;
    if !matches!(blob.get_type(), BlobType::OsmData) { continue; }
    let index = blob.index();
    // ... decide whether to decode based on index comparison ...
}
```

### Step 2: Block-pair alignment

The merge-join needs to align blocks from old and new. In a sorted PBF,
blocks are ordered by element type (nodes → ways → relations) and by ID
within each type. Two PBFs from the same source typically have identical
block boundaries.

**Alignment strategy:**

```
old_idx = old_cursor.peek_index()
new_idx = new_cursor.peek_index()

match (old_idx, new_idx):
  // Both exhausted
  (None, None) → phase done

  // One exhausted → all remaining are OldOnly or NewOnly
  (Some(_), None) → drain old as OldOnly (count from indexdata)
  (None, Some(_)) → drain new as NewOnly (count from indexdata)

  // Both have blocks
  (Some(o), Some(n)):
    if o.max_id < n.min_id:
      // Old block entirely before new → all OldOnly
      emit_all_old_only(o.count)
      old_cursor.skip_blob()
    elif n.max_id < o.min_id:
      // New block entirely before old → all NewOnly
      emit_all_new_only(n.count)
      new_cursor.skip_blob()
    else:
      // Overlapping ranges → might be same block or different
      if try_compressed_byte_equal(&old_blob, &new_blob):
        emit_all_equal(o.count)
        old_cursor.advance()
        new_cursor.advance()
      else:
        // Decode both and element-merge
        element_merge(old_cursor.decode(), new_cursor.decode())
```

**Edge case: misaligned block boundaries**

If the two PBFs have different block sizes (e.g., one was re-blocked),
a single old block might overlap multiple new blocks or vice versa.

Handle by decoding the smaller side and buffering, then advancing the
other side until the overlap is consumed. This is equivalent to the
current element-level merge but only triggered for the misaligned
portion.

For the common case (same source, same block boundaries), this never
triggers.

### Step 3: Element-level merge for overlapping decoded blocks

When blocks must be decoded, use borrowed elements instead of owned.
Both blocks are alive simultaneously (old_block, new_block), so
element references can borrow from them:

```rust
fn element_merge_blocks<F>(
    old_block: &PrimitiveBlock,
    new_block: &PrimitiveBlock,
    on_action: &mut F,
) -> Result<()>
where F: FnMut(MergeJoinAction<...>) -> Result<()>
{
    let mut old_iter = old_block.elements().peekable();
    let mut new_iter = new_block.elements().peekable();
    // Two-pointer merge on borrowed elements — no String allocation
    // Tags compared via iterator (TagIter) without collecting to Vec
}
```

**Tag equality without String allocation:**

Current `nodes_equal` compares `a.tags == b.tags` where tags is
`Vec<(String, String)>`. With borrowed elements, compare via iterators:

```rust
fn tags_equal(a_tags: TagIter<'_>, b_tags: TagIter<'_>) -> bool {
    a_tags.zip(b_tags).all(|((ak, av), (bk, bv))| ak == bk && av == bv)
    // But also need to check same length...
}
```

Actually, `TagIter` yields `(&str, &str)` — string comparison without
allocation. The `&str` borrows from the block's string table.

For length check, use `Iterator::eq()` which handles length mismatch:

```rust
fn tags_equal<'a>(mut a: TagIter<'a>, mut b: TagIter<'a>) -> bool {
    loop {
        match (a.next(), b.next()) {
            (Some((ak, av)), Some((bk, bv))) => {
                if ak != bk || av != bv { return false; }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}
```

Way refs: `a.refs().eq(b.refs())` — iterator comparison, no Vec.
Relation members: compare id + role via iterators. Role comparison
borrows from string table — no String allocation.

### Step 4: Integration with diff and derive_changes

Replace `merge_join_phase` with `block_pair_merge_join_phase`:

```rust
pub(crate) fn block_pair_merge_join_phase(
    old_reader: &mut BlobReader<FileReader>,
    new_reader: &mut BlobReader<FileReader>,
    type_filter: ElemKind,
    on_action: impl FnMut(BlockMergeAction) -> Result<()>,
) -> Result<()>;

pub(crate) enum BlockMergeAction {
    /// All elements in this blob are unchanged. Count provided.
    BlobEqual(u64),
    /// All elements in this blob exist only in old. Count provided.
    BlobOldOnly(u64),
    /// All elements in this blob exist only in new. Count provided.
    BlobNewOnly(u64),
    /// Individual element comparison result (decoded blocks).
    Element(ElementMergeAction),
}
```

For `diff`: `BlobEqual` increments `stats.common` by count.
`BlobOldOnly` increments `stats.deleted`. `BlobNewOnly` increments
`stats.created`. `Element(Modified/Equal/...)` works as before.

For `derive_changes`: `BlobEqual` is ignored (no output needed).
`BlobOldOnly` needs element-level access for delete output
(element ID + type needed for OSC XML). This means we can't skip
decode for OldOnly/NewOnly in derive_changes — we need at least
the IDs. But we CAN skip tag String allocation for these cases
since derive_changes only writes IDs + metadata to the OSC.

### Step 5: Verbose diff detail for Modified elements

When `diff --verbose` is used, Modified elements need full tag
comparison output. The current `write_node_details`, `write_way_details`,
`write_relation_details` functions access owned element fields.

With borrowed elements, these functions would receive `Element<'_>`
references. Tag diff output can iterate TagIters without allocation.

## Rollout plan

**Updated priority:** At planet scale, ~86% of node blobs contain at
least one modification in a typical daily diff. Blob-level byte
comparison (v1) only skips ~14% of node blobs. The borrowed element
merge (v2) is the critical optimization — it eliminates String
allocation for 100% of elements in decoded blocks, which is 86% of
all node blobs.

Recommended order: **v2 first, then v1, then v3.**

1. **v2: Borrowed element merge** (highest impact) — Replace
   OwnedNode/Way/Relation with borrowed element iterators for the
   decoded-block path. Both old and new blocks are alive simultaneously,
   so elements can borrow from their respective blocks. Tags compared
   via `&str` iterators, refs via `Iterator::eq()`, members via
   iterator comparison. Zero String allocation.
   More invasive — changes `MergeJoinElement` trait and all consumers.
   But correctness is verifiable via `brokkr verify diff`.

2. **v1: Blob-level equal skip** — Compare compressed bytes for
   matching blocks. Skip decode for identical blobs. Falls through
   to v2's borrowed element merge for differing blobs. Most effective
   for way/relation blobs (sparser modifications). ~14% of node blobs
   skip at planet scale, but way/relation blob skip rate is much higher.

3. **v3: Non-overlapping block skip** — Use indexdata min/max ID
   to skip decode for blocks that are entirely OldOnly or NewOnly
   (different block boundaries between old and new).
   Handles the misaligned-boundary case efficiently.

## Risk assessment

- **v1 is very low risk** — additive optimization, falls through to
  existing code for any mismatch. Testable via `brokkr verify diff`.
- **v2 is medium risk** — changes the element comparison API, needs
  careful lifetime management. But correctness is verifiable.
- **v3 is low risk** — additive on top of v1, handles edge cases
  gracefully by falling through to element-level merge.

## Testing

- `brokkr verify diff` compares pbfhogg output against osmium diff.
- Additional: verify stats match between old and new implementations
  on Denmark/Japan/Europe.
- Edge cases: PBFs with different block boundaries, PBFs with mixed
  blocks, PBFs without indexdata (fall through to element-level).

## Review feedback (April 2026, 3 Opus reviewers)

### Correctness review

- **Compressed byte comparison:** Sound. No false positives possible.
  False negatives (different compression settings) are harmless (fall
  through). No issues.
- **Tag ordering:** Guaranteed within the same block encoding (protobuf
  field order is deterministic). Iterator-based comparison is equivalent
  to Vec equality. No regression from current behavior.
- **Missing indexdata:** IMPORTANT — the plan must explicitly handle
  PBFs without indexdata. When either blob lacks indexdata, skip all
  blob-level optimizations and decode both for element-level merge.
  Add an explicit `if idx.is_none() { decode_both }` guard.
- **derive_changes OldOnly/NewOnly:** Count-only `BlobOldOnly` does
  NOT work for derive_changes deletes — element IDs are needed for
  OSC XML. Must decode for derive_changes even when diff can skip.
  Diff and derive_changes need different `BlockMergeAction` handling.

### Lifetime review

- **Two PrimitiveBlocks simultaneously:** SOLVABLE. Separate owned
  values, immutable borrows, no conflict.
- **Cross-lifetime tag comparison:** SOLVABLE. `str::eq` erases
  lifetimes at the comparison site. `tags_equal` function signature
  should use two distinct lifetimes (not a single `'a`).
- **MergeJoinElement trait:** Can be dropped entirely. Direct
  `element_merge_blocks` function with `match` on Element variants.
- **DenseNode vs Node:** TRICKY but manageable. Both tag iterators
  yield `(&str, &str)`. Need to handle DenseNode-vs-Node cross-match
  (rare but possible).
- **Misaligned block boundaries:** TRICKY. Holding one block alive
  across multiple iterations of the other side requires `Option`-based
  state management. Cleanest approach: keep the "larger" decoded block
  in an `Option`, iterate new blocks against it until the old block's
  ID range is fully consumed, then drop it and decode the next.
