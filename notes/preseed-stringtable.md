# Pre-seed StringTable / index passthrough investigation

Investigation of the remaining StringTable optimization opportunity in merge's
`rewrite_block`: avoiding string re-interning for unmodified base elements by
preserving input string table indices in the output.

Builds on: `notes/rewrite-block-cost-breakdown.md` (StringTable cost analysis),
the `get()` fast-path optimization (commit f5c5674), and the take buffer reuse
(commit 9f3ab14).

## Context: where we are after the get() fast-path

The `get()` fast-path eliminated allocation on cache hits (~99% of `add()` calls)
but the per-call cost is still ~10ns (FxHash + probe). At 131M `add()` calls on
Denmark cat, that's ~1.3s of hash+probe work. For merge's `rewrite_block` subset
(~11M calls across 630 blocks), it's ~110ms.

Post-optimization profiled timings (commit f5c5674, Denmark):

| Function          | Calls     | Avg   | Total  |
|-------------------|-----------|-------|--------|
| add_node          | 2.6M      | 41ns  | 107ms  |
| add_way           | 2.4M      | 219ns | 526ms  |
| add_relation      | 46K       | 491ns | 23ms   |
| **add_* total**   |           |       | **656ms** |
| take              | 7407      | ~91µs | ~674ms |
| rewrite_block     | 630       |       | 2.03s  |

StringTable::add's share of add_*: still ~60-80% (down from ~80-90% pre-fast-path
but still dominant). The remaining add_* time is delta encoding, Vec pushes, and
proto struct construction.

## The opportunity: skip string interning entirely for base elements

>99.9% of elements in rewritten blocks are unmodified base elements being
round-tripped. Their strings (tag keys, tag values, member roles, user names)
already exist in the **input** block's string table. If the output block used
the same string table with the same indices, these elements could write their
string table indices directly — no hashing, no probing, no allocation.

### What the read side already exposes

The read API already has raw index access:

| Element type | Tags (raw)          | Member roles (raw) | User (raw)          |
|--------------|---------------------|--------------------|---------------------|
| DenseNode    | `raw_tags()` → `(i32, i32)` | N/A         | `DenseNodeInfo.user_sid: i32` (private) |
| Node         | `raw_tags()` → `(u32, u32)` | N/A         | `WireInfo.user_sid: Option<i32>` (crate) |
| Way          | `raw_tags()` → `(u32, u32)` | N/A         | `WireInfo.user_sid: Option<i32>` (crate) |
| Relation     | `raw_tags()` → `(u32, u32)` | `RelMember.role_sid: i32` (public) | `WireInfo.user_sid: Option<i32>` (crate) |

Raw tag iterators exist on all element types. `RelMember.role_sid` is already
public. `user_sid` is accessible at the wire/info struct level (crate-internal).

The `WireStringTable` stores entries as `(offset: u32, length: u32)` pairs
referencing the decompressed buffer. It has `get(index) -> Option<&[u8]>` and
`len() -> usize`.

### What the write side currently requires

All `BlockBuilder::add_*` methods accept `&str` for tag keys, tag values, member
roles, and user names. Internally they call `StringTable::add(&str) -> u32` to
intern strings. There is no way to insert raw indices.

The full call chain for a base way in merge:

```
input PrimitiveBlock
  → Way.tags() → TagIter → (key: &str, val: &str)  [decodes u32 → &str]
  → tags_buf.extend(...)
  → bb.add_way(id, &tags_buf, &refs_buf, meta)
    → StringTable::add(key) → u32  [hashes &str → index]
    → StringTable::add(val) → u32
    → StringTable::add(meta.user) → u32
```

Three decode+re-encode cycles per string. The goal is to short-circuit this to:

```
input PrimitiveBlock
  → Way.raw_tags() → RawTagIter → (key_idx: u32, val_idx: u32)  [no decode]
  → bb.add_way_raw(id, raw_tags, &refs_buf, raw_user_sid)
    → remap[key_idx] → output index  [array lookup, no hash]
    → remap[val_idx] → output index
    → remap[raw_user_sid] → output index
```

## Design options

### Option A: Pre-seed StringTable, remap indices

**Concept:** At the start of rewrite_block, copy the input block's string table
entries into the output BlockBuilder's StringTable. Build a remap array:
`input_index → output_index`. For base elements, use `raw_tags()` + remap. For
diff elements, use normal `add(&str)`.

**Pre-seed cost:** A typical block has ~1200 unique strings. Pre-seeding requires:
1. Iterate WireStringTable entries: 1200 × `get(i)` → `&[u8]` → `from_utf8_unchecked` → `&str`
2. Call `StringTable::add(&str)` for each → allocates String on first occurrence
3. Build remap: `Vec<u32>` of length 1200

The 1200 `add()` calls allocate 1200 Strings (~18KB total) and do 1200 hash+insert
operations. This is a one-time cost per block, amortized over ~7000 elements/block.

Per-block overhead: ~1200 × 27ns (old) or ~1200 × 10ns (new get fast-path, all misses
since table is empty) ≈ 12-32µs per block × 630 blocks = 8-20ms total. Negligible.

**Remap usage:** For base elements, replace `StringTable::add(&str)` calls with
`remap[input_index]` array lookups. Cost: ~1-2ns per lookup (L1 cache hit for
1200-entry u32 array = 4.8KB).

**Diff element handling:** The ~0.1% of elements from the diff use `add(&str)`
normally. Their strings may or may not already exist in the pre-seeded table.
No special handling needed — the existing fast-path `get()` handles this.

**Savings estimate:**

Per base element, the StringTable cost changes from:
- Before: N × (FxHash + probe) = N × ~10ns
- After: N × (array index) = N × ~2ns

Where N = add() calls per element (1.6 nodes, 7 ways, 17 relations).

| Element   | Calls/elem | Old cost | New cost | Δ/elem |
|-----------|-----------|----------|----------|--------|
| add_node  | 1.6       | 16ns     | 3.2ns    | -13ns  |
| add_way   | 7         | 70ns     | 14ns     | -56ns  |
| add_relation | 17     | 170ns    | 34ns     | -136ns |

Total savings across 4.4M elements in rewrite_block:
- Nodes: 2.6M × 13ns = 34ms
- Ways: 2.4M × 56ns = 134ms
- Relations: 46K × 136ns = 6ms
- **Total: ~174ms** (~8.6% of rewrite_block's 2.03s)

Plus: eliminates the `str_from_stringtable()` decode cost on the read side (~5ns
per call, same call count) → additional ~55ms.

**Estimated total: ~230ms (11% of rewrite_block).**

### Option B: Index passthrough mode (no re-interning at all)

**Concept:** For base elements, skip StringTable entirely. Copy the input block's
string table wholesale into the output block, then write raw indices directly
into the proto fields without any StringTable involvement.

**How it works:**
1. Copy input `WireStringTable` entries directly into proto `StringTable.s` field
2. For base elements: write raw indices from `raw_tags()` directly into
   `way.keys`, `way.vals`, `rel.roles_sid`, `dense_keys_vals`, etc.
3. For diff elements: need to extend the string table and use new indices

**Advantage over Option A:** Zero hash operations for base elements. The remap
array from Option A is identity (input index = output index) since we copy the
string table verbatim. So `remap[i] = i` — we skip the remap entirely.

**Critical complication — mixed blocks:** A rewritten block may contain both base
elements and diff elements in the same block. Diff elements may introduce new
strings not in the input string table. These new strings must be appended to the
output string table (at indices beyond the input table's range).

This means the output block's string table = input string table ++ new strings
from diff. Input indices are valid as-is (identity mapping). New strings from
diff get indices starting at `input_table.len()`.

This works naturally if:
1. Pre-load input string table into output StringTable (same as Option A pre-seed)
2. Diff element `add()` calls find existing strings via get() fast-path or insert
   at the end
3. Base elements use raw indices directly (they're valid because the input table
   is a prefix of the output table)

Wait — this IS Option A. The only difference is whether we do the remap lookup
or not. If the output table starts with the same entries in the same order as the
input table, then `remap[i] = i` for all pre-seeded strings, and we can skip the
remap entirely.

**Can we guarantee identity mapping?** Yes, if we pre-seed by iterating the input
table in order (index 0, 1, 2, ...) and the output StringTable is empty at the
start. Each `add()` call returns the next sequential index, matching the input.

So Option B collapses to: **Option A with identity remap, where base elements
write raw indices directly instead of doing remap[i].**

### Option C: Dual-mode BlockBuilder (raw index API)

**Concept:** Add new `add_node_raw`, `add_way_raw`, `add_relation_raw` methods
to BlockBuilder that accept raw string table indices instead of `&str`.

```rust
pub fn add_way_raw(
    &mut self,
    id: i64,
    raw_tag_keys: &[u32],
    raw_tag_vals: &[u32],
    refs: &[i64],
    raw_user_sid: Option<u32>,
    // ... other metadata fields (non-string, passed directly)
) { ... }
```

**Advantage:** Cleanest separation. Base elements go through the raw path with
zero hashing. Diff elements go through the normal `add(&str)` path.

**Disadvantage:** Duplicates the add_* logic (delta encoding, capacity checks,
proto construction) for each element type. 6 new methods (3 types × 2 modes).

**Alternative: make StringTable::add accept both modes:**

Rather than duplicating the entire add_* methods, we could add a method to
StringTable that directly inserts at a known index:

```rust
fn add_raw(&mut self, idx: u32) -> u32 {
    idx  // Identity — string already in table at this index
}
```

But this doesn't work because the base element path still needs to push indices
into the same fields (`dense_keys_vals`, `way.keys`, etc.). The methods already
do this — they just call `string_table.add(s)` to get the index. If we could
substitute `add_raw(idx) -> idx` for `add(s) -> idx`, the rest of the method
is identical.

**Better alternative: make the callers (merge) handle the optimization
externally.** Instead of changing BlockBuilder's API, change merge's
`write_base_*` functions to:
1. Pre-seed the StringTable at block start (one-time)
2. Use `raw_tags()` + identity mapping to produce `Vec<(&str, &str)>` more
   efficiently... wait, that still decodes to `&str`.

Actually, the cleanest approach: **add `add_raw_tag_indices` support** to the
existing add_* methods via a trait or mode flag. But this adds complexity to
the public API for a merge-only optimization.

## Recommended approach: Option A (pre-seed + remap)

Option A is the pragmatic choice:

1. **Minimal API change**: Add one method to BlockBuilder:
   `pre_seed_string_table(entries: impl Iterator<Item = &str>)`
2. **Merge-only change**: Only `write_base_*` in merge.rs changes to use
   `raw_tags()` + remap instead of `tags()` + `add()`
3. **Correctness trivially verifiable**: The remap array is identity when
   pre-seeded from the input table (every existing test still passes)
4. **Fallback graceful**: If pre-seed wasn't called, remap is empty, code
   falls back to normal add(&str) path — no behavioral change for other commands

### Implementation sketch

**Step 1: Add pre-seed to StringTable**

```rust
impl StringTable {
    /// Pre-seed from an input string table, returning a remap array.
    /// remap[input_index] = output_index (identity when table was empty).
    fn pre_seed(&mut self, entries: &WireStringTable<'_>) -> Vec<u32> {
        let mut remap = Vec::with_capacity(entries.len());
        for i in 0..entries.len() {
            let s = std::str::from_utf8(entries.get(i).unwrap_or(b""))
                .unwrap_or("");
            remap.push(self.add(s));
        }
        remap
    }
}
```

Wait — this still calls `add()` which allocates a String for every entry
(all are misses on an empty table). That's 1200 String allocs per block.
Compare to the current code which does ~7000 elements × 2.22 add() calls
= ~15,500 add() calls per block, of which ~14,300 are cache hits (free
after get() fast-path) and ~1200 are misses. So pre-seeding does NOT save
any allocations — the same ~1200 unique strings are allocated either way.

**The saving is purely on the per-element lookup side:** replacing ~14,300
hash+probe operations (~10ns each = ~143µs per block) with ~14,300 array
index lookups (~2ns each = ~29µs per block). Net per-block saving: ~114µs.
Over 630 blocks: ~72ms.

Hmm, that's lower than my earlier estimate. Let me reconcile.

### Reconciled cost model

The earlier estimate of ~230ms assumed eliminating both the hash+probe cost
AND the str_from_stringtable decode cost. Let me break this down properly:

**Current cost per base element (way, the dominant case):**

1. Read side: `way.tags()` iterates `TagIter`:
   - Per tag: 2 varint decodes (~2ns each) + 2 `str_from_stringtable()` calls
   - `str_from_stringtable`: array index into entries + bounds check + ptr math → ~3ns
   - Per tag total: ~10ns read cost

2. Buffer: `tags_buf.extend(way.tags())` — collect &str pairs, no alloc (reused Vec)

3. Write side: `bb.add_way(id, &tags_buf, &refs_buf, meta)`
   - Per tag: 2 `StringTable::add()` calls
   - `add()` (post-fast-path): FxHash compute + probe → ~10ns per call
   - Per tag total: ~20ns write cost

4. Metadata: `element_metadata()` → `build_info()` → 1 `add(user)` → ~10ns

Per way with ~3 tags: 6 × 10ns (read) + 6 × 10ns (write) + 10ns (user) = ~130ns
for string-related work. add_way total is 219ns, so ~60% is string-related.

**With pre-seed + raw indices:**

1. Read side: `way.raw_tags()` iterates `RawTagIter`:
   - Per tag: 2 varint decodes (~2ns each), NO stringtable lookup
   - Per tag total: ~4ns read cost

2. No buffer collection needed for string data (raw indices are u32)

3. Write side: array index into remap (identity):
   - Per tag: 2 array lookups → ~2ns per call
   - Per tag total: ~4ns write cost

4. Metadata user: remap[user_sid] → ~2ns

Per way with ~3 tags: 6 × 4ns (read) + 6 × 4ns (write) + 2ns (user) = ~50ns
for string-related work.

**Saving per way: ~80ns** (130ns → 50ns).

2.4M ways × 80ns = **192ms**. This is the dominant saving.

For nodes (~1.6 string ops/node):
- Old: ~16ns string cost, New: ~6ns, Δ = 10ns × 2.6M = **26ms**
For relations (~17 string ops/rel):
- Old: ~170ns, New: ~36ns, Δ = 134ns × 46K = **6ms**

**Total estimated saving: ~224ms (~11% of rewrite_block's 2.03s).**

This is roughly **6.5% of merge wall time** (assuming rewrite_block is ~57%
of wall). At planet scale (80× Denmark), that's ~18s saved.

### The catch: is 224ms worth the complexity?

The implementation requires:
1. A `pre_seed` method on StringTable (or BlockBuilder)
2. Exposing `WireStringTable` or its entries to the merge code
3. New `write_base_*_raw` functions (or modifying existing ones) that use
   `raw_tags()` + remap instead of `tags()` + `add()`
4. Handling dense node tags differently (interleaved `keys_vals` with 0
   delimiters — need to write raw i32 indices directly)
5. Handling metadata user_sid (currently goes through `Metadata<'a>` struct
   with `user: &str`)
6. Testing that remap produces identical output

**Lines of code estimate:** ~200-300 lines changed across 3 files
(block_builder.rs, merge.rs, mod.rs). Not trivial but not massive.

**Risk:** Low. Pre-seeded blocks produce byte-identical output when the remap
is identity. The roundtrip tests already validate PBF correctness.

## Alternative: avoid the remap entirely

Since pre-seeding from an empty table produces identity mapping (input index
= output index), we don't need a remap array at all. We just need to:

1. Pre-seed the output StringTable from the input table (1200 `add()` calls)
2. For base elements: write raw input indices directly — they're valid because
   the output table has the same entries in the same order
3. For diff elements: use normal `add(&str)` which may extend the table

The only risk: a diff element could introduce a string that's already in the
pre-seeded table. `add()` would find it via get() fast-path and return the
pre-seeded index. No problem — this is handled automatically.

The other risk: the output table could have entries at different indices if
StringTable::add() doesn't preserve insertion order. But it does: index =
`strings.len()`, which increments sequentially. Pre-seeding in order from
index 0 produces identity mapping.

**Even simpler: don't even pre-seed.** Just write raw indices from the input
table, and separately ensure the output string table contains the same
entries. The only constraint is: at the time of `take()`, the output
StringTable must be a superset of the input StringTable, with input entries
at the same indices.

Pre-seeding guarantees this. The question is whether the ~1200 allocations
per block (for pre-seeding) are worth avoiding. They're not avoidable with
the current StringTable design (needs owned Strings) — but they happen
regardless today (the first ~1200 elements populate the table anyway).

So pre-seeding shifts the allocations to block start, but doesn't change
the total count. The only cost difference vs today is the ~630 blocks ×
~1200 entries × 10ns = ~8ms of hash+insert for the pre-seed itself. This is
already included in the "current cost" since the same insertions would happen
as elements are processed.

**Conclusion: pre-seeding is essentially free** — it front-loads work that
would happen anyway. The win comes entirely from the base element path
switching from `tags()` + `add()` to `raw_tags()` + direct index writes.

## Implementation requirements

### BlockBuilder changes

1. **New method: `pre_seed_string_table(table: &WireStringTable<'_>)`**
   - Iterates input table entries (0..len)
   - Calls `add()` for each (populates the FxHashMap for later diff lookups)
   - Stores input table length for validation

2. **New method: `add_node_raw_tags(id, lat, lon, raw_tags: &[(i32, i32)], raw_user_sid, non_string_meta)`**
   - Same as `add_node` but writes raw tag indices directly to `dense_keys_vals`
   - Skips `StringTable::add()` for tags and user
   - Still does delta encoding for coordinates (unchanged)
   - Need to handle the 0-delimiter for dense node tags

3. **New method: `add_way_raw_tags(id, raw_tag_keys, raw_tag_vals, refs, raw_user_sid, non_string_meta)`**
   - Same as `add_way` but writes raw indices directly to `way.keys` / `way.vals`
   - Skips `StringTable::add()` for tags and user

4. **New method: `add_relation_raw_tags(id, raw_tag_keys, raw_tag_vals, raw_role_sids, memids, types, raw_user_sid, non_string_meta)`**
   - Same as `add_relation` but writes raw indices directly

### Merge changes

1. **In `rewrite_block()`:** call `bb.pre_seed_string_table(&block.string_table())`
   at block start
2. **`write_base_dense_node()`:** use `dn.raw_tags()` + `dn.info().user_sid` +
   new `add_node_raw_tags()`
3. **`write_base_way()`:** use `way.raw_tags()` + `way.info().user_sid` +
   new `add_way_raw_tags()`
4. **`write_base_relation()`:** use `rel.raw_tags()` + `RelMember.role_sid` +
   `rel.info().user_sid` + new `add_relation_raw_tags()`
5. **Diff element paths (write_osc_*):** unchanged, use normal `add(&str)`

### Read-side changes needed

1. **Expose `WireStringTable` or a way to iterate entries:** Currently crate-internal.
   Need at minimum a method on PrimitiveBlock like:
   `pub fn string_table_len(&self) -> usize`
   `pub fn string_table_entry(&self, index: usize) -> Option<&str>`
   Or expose a `StringTableIter` that yields `&str` entries in order.

2. **Expose `user_sid` on Info/DenseNodeInfo:** Currently only accessible as
   decoded `&str` via `.user()`. Need to expose the raw `i32` index.
   - `DenseNodeInfo.user_sid` is private (field) — add a public getter
   - `Info.user_sid` delegates to `WireInfo.user_sid` — add a public getter

### What NOT to expose

- Do NOT expose `WireStringTable` itself — it's an implementation detail
- Do NOT change the public API of `add_node`/`add_way`/`add_relation` — those
  remain the stable API for general use. The raw variants are merge-internal.
- The raw variants could be `pub(crate)` if we don't want to commit to them
  as public API.

## Summary

| Aspect              | Value                    |
|---------------------|--------------------------|
| Estimated saving    | ~224ms on Denmark merge  |
| % of rewrite_block  | ~11%                    |
| % of merge wall     | ~6.5%                  |
| Planet-scale saving | ~18s (extrapolated)     |
| Lines changed       | ~200-300                |
| Files changed       | 3-4 (block_builder.rs, merge.rs, elements.rs, dense.rs) |
| Risk                | Low (identity remap, roundtrip-tested) |
| Benefits other cmds | No (merge-only optimization) |

### Comparison with completed optimizations

| Optimization              | Saving (merge) | Complexity |
|---------------------------|----------------|------------|
| take buffer reuse         | ~960 MB alloc  | Low (3 lines + API change) |
| StringTable get() fast-path | 2.17→2.03s  | Low (3 lines) |
| **Pre-seed + raw indices** | **~224ms**   | **Medium (200-300 lines)** |

### Verdict

**Worth doing, but moderate bang-for-buck.** The saving is real (~224ms on
Denmark, ~18s at planet scale) but requires non-trivial changes across 3-4
files. The per-element saving is ~80ns for ways (the dominant case), which
is meaningful at 2.4M elements.

The main risk is API complexity — adding raw-index methods to BlockBuilder
creates a parallel code path that must be kept in sync with the standard
methods. Using `pub(crate)` visibility limits the maintenance burden.

If implemented, the next profiling target becomes the non-string costs in
`add_way` (delta encoding, Vec pushes, proto construction) at ~90ns/call,
and `take()` serialization at ~91µs/block. These are harder to optimize
and have diminishing returns.
