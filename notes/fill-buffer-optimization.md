# fill_buffer / stream_merge allocation optimization

## Problem

`fill_buffer` in `src/commands/stream_merge.rs` generates 80.7 GB
cumulative alloc on Japan for the `diff` command (commit `bb15e66`).
The allocation comes from materializing every PBF element into owned
structs (`OwnedNode`/`OwnedWay`/`OwnedRelation`) with `String` clones
for every tag key and value.

## Root cause

The merge-join needs owned elements because it holds two elements
simultaneously (old and new) for comparison, and they come from
different `PrimitiveBlock`s that can't both be alive at once. The
current flow:

1. `fill_buffer` decodes a block into `Vec<OwnedNode>` (one block's
   worth, ~8000 elements)
2. `next_element` pops one at a time for the two-pointer merge-join
3. `merge_join_phase` compares old.id() vs new.id()
4. On equal IDs, calls `T::equal()` which compares tags, coords,
   refs, members
5. Callback receives the action (Equal/Modified/OldOnly/NewOnly)
6. Element is dropped, its Strings freed

**Key observation:** for `diff`, the vast majority of elements are
`Equal` (unchanged between the two PBFs). A typical daily diff on
Denmark modifies ~645K elements out of ~54M — **98.8% are unchanged**.
At planet scale: ~5-30M changes out of ~11.6B elements — **99.7%+
unchanged** at the element level.

**Critical caveat (planet-scale):** although only ~0.3% of elements
change, the modifications are geographically dispersed — ~86% of
node blobs contain at least one modified node in a typical daily
planet diff. This means blob-level compressed byte comparison (approach
2) can only skip ~14% of node blobs. The element-level merge still
processes ~86% of blobs. The block-pair merge-join (approach 4) with
ID range comparison helps more: non-overlapping blocks between old
and new PBFs can be skipped without decode even if they contain
modifications (because the modifications are handled by a different
overlapping block pair).

For unchanged elements, we allocate 2 Strings per tag on both sides,
compare them byte-by-byte, find them equal, and immediately free
everything. This is pure waste.

## Approach 1: Raw byte comparison (fast path for Equal)

If the raw protobuf bytes for an element are identical between old
and new, the element is unchanged — no need to parse tags/refs/members.
This requires aligning the comparison at the wire-format level.

### Challenges

PBF elements within a PrimitiveBlock use **string table indices**, not
inline strings. Two blocks can encode the same element with different
string table layouts (different index assignments), making raw byte
comparison unsound for elements with tags.

Tagless nodes (the majority) encode as: `[sint64 delta_id, sint64
delta_lat, sint64 delta_lon]` in the DenseNodes packed arrays. These
use delta encoding relative to the previous element in the block, so
raw comparison only works for the first element in each block (or if
blocks are identically structured).

**Verdict: not feasible at the element level.** The delta encoding
and string table indirection make raw comparison unreliable.

## Approach 2: Raw block-level comparison

If an entire compressed blob is identical between old and new (same
bytes on disk), all elements in that blob are unchanged. This is a
valid fast path:

1. For each blob, compare the compressed bytes (or xxhash)
2. If identical: skip decode, emit all elements as `Equal`
3. If different: fall through to element-level comparison

At planet scale with a daily diff, ~99%+ of blobs are unchanged.
This eliminates nearly all allocation.

### Implementation sketch

The `StreamingBlocks` cursor already has access to blobs before decode.
Add a `next_blob_raw()` method that returns the raw compressed bytes
alongside the blob. In the merge-join, before calling `fill_buffer`,
compare blob compressed bytes:

```
loop {
    let (old_raw, old_block) = old_src.next_blob_with_raw()?;
    let (new_raw, new_block) = new_src.next_blob_with_raw()?;
    if old_raw == new_raw {
        // Entire blob unchanged — count elements, skip decode
        stats.common += blob_element_count;  // from indexdata
        continue;
    }
    // Fall through to element-level merge
}
```

### Challenge: block alignment

The two PBFs must have identical block boundaries for blob-level
comparison. This is true when:
- Both were written by the same tool with the same settings
- Neither has been re-sorted or re-blocked

For `diff` (comparing two snapshots of the same region), block
alignment is **not guaranteed** — a `sort` or `cat` operation may
re-block the data. The approach is sound as an optimization but
not a correctness requirement — misaligned blobs fall through to
element-level comparison.

For `apply-changes` (merge), the base PBF's blocks are preserved
for passthrough, so block alignment is inherent.

## Approach 3: Lazy owned elements (decode tags on demand)

Change `OwnedNode`/`OwnedWay`/`OwnedRelation` to defer tag String
allocation. Store a reference to the block's string table + tag
indices instead of cloned Strings. Only materialize Strings when
`equal()` needs to compare tags (which still requires String
comparison — or string table index comparison if the blocks share
a table).

### Lifetime problem

The owned element must outlive the `PrimitiveBlock` it came from
(the buffer holds elements across block boundaries). This requires
either:
- Keeping the PrimitiveBlock alive while its elements are buffered
  (changes the buffer from `Vec<OwnedNode>` to a block+elements pair)
- Copying the raw tag bytes (not Strings) into the owned element —
  smaller than String but still an allocation

### Variant: copy raw tag bytes, defer parsing

Store the raw tag field bytes (packed varint string table indices)
plus a copy of the string table in the owned element. Comparison
can then happen on the raw bytes first (fast path if identical),
with String materialization only for the verbose diff output.

**Estimated savings:** For a typical element with 5 tags averaging
10 bytes each, raw tag bytes ≈ 20 bytes (5 × 2 varint indices)
vs 100 bytes in 10 String allocations. 5x reduction in tag storage,
plus zero String allocation for the Equal path.

## Approach 4: Block-pair merge-join

Instead of element-level cursor, operate at block-pair level:

1. Read one block from each source
2. If blocks have non-overlapping ID ranges: entire block is
   OldOnly or NewOnly (no element decode needed for diff counting)
3. If blocks overlap: decode both, merge-join elements within the
   block pair
4. Elements only need to live within the scope of one block pair
   — no cross-block ownership needed

This eliminates the `Vec<OwnedNode>` buffer entirely for the
non-overlapping case, and bounds element lifetime to a single
function scope for the overlapping case (enabling borrowed
elements with block references).

### ID range from indexdata

Indexed PBFs store min_id/max_id per blob. For sorted PBFs with
indexdata, block-pair alignment and ID range comparison is O(1)
per blob — no decompression needed.

## Recommendation

**Approach 4 (block-pair merge-join)** is the most promising:
- Eliminates allocation for non-overlapping blocks (majority case)
- Enables borrowed elements for overlapping blocks (no Strings)
- Works naturally with the existing indexdata infrastructure
- No correctness risk (falls through to element-level for all cases)

**Approach 2 (raw block comparison)** is a complementary fast path
that can be added independently. Even without block alignment,
the hash/byte comparison is cheap and catches the common case.

Approach 3 is the fallback for elements that must be materialized.

## Impact estimate

- Japan diff: 80.7 GB → ~1 GB (98.8% elements unchanged, skip decode)
- Planet diff: more nuanced than initially estimated. ~99.7% of elements
  are unchanged, but ~86% of node blobs contain at least one change.
  Blob-level byte comparison (approach 2) only skips ~14% of node blobs.
  The real wins come from:
  1. **Borrowed element merge (approach 4, v2):** eliminates String
     allocation for all elements in decoded blocks. This is the biggest
     win — tags compared via `&str` iterators instead of
     `Vec<(String, String)>`. Affects 100% of decoded elements.
  2. **Metadata skip:** the current path allocates OwnedMetadata for
     every element but equality checks don't use it. Skipping metadata
     saves ~5 allocations per element.
  3. **Blob-level skip (approach 2/4, v1):** still valuable for way and
     relation blobs where modifications are sparser (typical daily diff
     modifies far fewer way/relation blobs than node blobs).
- Wall time: String allocation + comparison may be more significant
  than initially assumed if 86% of node blobs need element-level merge.
  The borrowed element approach (zero String allocation) becomes the
  critical optimization, not blob skipping.
