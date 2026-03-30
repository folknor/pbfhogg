# BlockBuilder Tag/Member API: Iterator vs Slice

## Problem

`add_node`/`add_way`/`add_relation` take `tags: &[(&str, &str)]` and
`members: &[MemberData<'_>]` slices. Callers that don't already have a
slice must `.collect()` into a temporary Vec per element. At planet scale
in sort/merge_pbf sweep merge, this is ~10B per-element allocations.

Measured: `fill_buffer` = 80.7 GB cumulative alloc on Japan (diff command,
commit `ea1ab6e`). The `write_single_*` functions in `elements_pbf.rs`
allocate `Vec<(&str, &str)>` per call. Scratch reuse fails because `&str`
borrows from different `OwnedNode`/`OwnedWay` each iteration (lifetime
mismatch across calls).

## Current internal structure

`encode_way` and `encode_relation` iterate tags **twice** — once for keys
(packed field 2), once for values (packed field 3). `encode_relation`
iterates members **three times** — roles (field 8), memids (field 9),
types (field 10). This two/three-pass structure is why a single-pass
iterator can't simply replace the slice parameter.

`add_node` (dense path) iterates tags **once** — interleaved
`[key_sid, val_sid, ..., 0]`. No two-pass issue.

## Approaches considered

### Approach 1: Iterator-based public API

Change `add_node`/`add_way`/`add_relation` to accept
`impl Iterator<Item = (&str, &str)> + Clone` for tags and
`impl Iterator<Item = MemberData<'_>> + Clone` for members.

Internally: clone the iterator for the second/third pass. Clone is cheap
for typical iterators (slice::iter, Map, etc.).

Pros:
- Eliminates the intermediate Vec at every call site
- Clean API — callers pass iterators naturally
- No internal scratch needed for the two-pass issue

Cons:
- ~50 call sites to update (mechanical)
- `Clone` bound is a requirement on callers
- Most callers currently pass `&tags_buf` (a slice) — they'd change to
  `tags_buf.iter().copied()` or similar

### Approach 2: Internal index collection

Keep `tags: &[(&str, &str)]` public API. Inside `encode_way`/
`encode_relation`, collect string table indices `(u32, u32)` into a
scratch Vec on the first pass, then replay indices for the second pass.

Pros:
- No API change, no caller updates
- Scratch Vec is `(u32, u32)` — no lifetime issues, reusable
- Reduces string table lookups from 2× to 1× per tag

Cons:
- Doesn't eliminate the `write_single_*` Vec allocation (caller-side)
- Needs a separate fix for `elements_pbf.rs` (e.g., `add_way_owned`
  accepting `&[(String, String)]`)

### Approach 3: SmallVec

Use `SmallVec<[(&str, &str); 16]>` in `write_single_*` to stack-allocate
for typical tag counts.

Pros:
- Avoids heap for <16 tags (common case)
- Minimal code change

Cons:
- Adds a dependency
- Doesn't fix elements with >16 tags
- Doesn't address the structural two-pass issue
- Band-aid, not a fix

**Rejected by all 6 reviewers.**

### Approach 4: Dual packed buffer (single pass)

Encode both packed key and value fields simultaneously into two separate
buffers in a single loop:

```rust
packed_keys.clear();
packed_vals.clear();
for &(key, val) in tags {
    let key_idx = string_table.add(key);
    tag_key_indices.insert(key_idx);
    encode_varint(packed_keys, u64::from(key_idx));
    encode_varint(packed_vals, u64::from(string_table.add(val)));
}
encode_bytes_field(elem, 2, packed_keys);
encode_bytes_field(elem, 3, packed_vals);
```

Pros:
- Single pass, zero index collection
- No intermediate scratch Vec for indices
- Works with both slice and iterator input

Cons:
- Needs one additional `Vec<u8>` on BlockBuilder (packed_vals buffer)
- Minor: two encode_bytes_field calls instead of interleaved

### Approach 5: Combined (recommended)

Approach 1 (iterator API) + Approach 4 (dual packed buffer).

Change the public API to iterators. Internally use dual packed buffers
for single-pass encoding. This gives:
- Zero per-element allocation at call sites (no .collect())
- Single-pass encoding (no clone needed if using dual buffers)
- Actually, with dual buffers, the Clone bound isn't needed at all —
  single pass handles both keys and values

This means the API becomes:
```rust
pub fn add_way(
    &mut self,
    id: i64,
    tags: impl Iterator<Item = (&str, &str)>,  // no Clone needed
    refs: &[i64],
    metadata: Option<&Metadata<'_>>,
)
```

No Clone. Single pass. Zero intermediate allocation. The dual packed
buffer handles the two-field encoding internally.

For `add_relation`, members need three fields — same dual/triple buffer
approach:
```rust
pub fn add_relation(
    &mut self,
    id: i64,
    tags: impl Iterator<Item = (&str, &str)>,
    members: impl Iterator<Item = MemberData<'_>>,
    metadata: Option<&Metadata<'_>>,
)
```

Members are encoded into three separate packed buffers (roles, memids,
types) in a single pass.

## Reviewer consensus (6/6)

- Don't use SmallVec
- Internal index collection (approach 2) is good cleanup regardless
- The `write_single_*` caller-side allocation is the real target
- Scratch should live on BlockBuilder (alongside existing elem_scratch)
- Same pattern applies to relation members (three-pass → single-pass)

## Decision

**Approach 5** (iterator API + dual packed buffer). We have no published
API — no reason to preserve the slice interface. The iterator API is
strictly better: zero allocation, single pass, no Clone needed with
dual buffers.

## Implementation plan

1. Add `packed_vals: Vec<u8>` to BlockBuilder (dual buffer for tag values)
2. Add `member_roles: Vec<u8>`, `member_ids: Vec<u8>`,
   `member_types: Vec<u8>` to BlockBuilder (triple buffer for members)
3. Change `add_node` tags param: `&[(&str, &str)]` → `impl Iterator<Item = (&str, &str)>`
4. Change `add_way` tags param: same
5. Change `add_way_with_locations` tags param: same
6. Change `add_relation` tags + members params: both to iterators
7. Update `encode_way`/`encode_relation` to single-pass dual/triple buffer
8. Update all ~50 call sites: `&tags_buf` → `tags_buf.iter().copied()`
   or `element.tags()` directly
9. Remove `.collect::<Vec<_>>()` from `write_single_*` — pass owned tag
   iterator directly
10. Update `add_*_raw` methods if they use slices (merge passthrough)

## Call site categories

Most callers fall into one of three patterns:

**Pattern A (majority):** `tags_buf.clear(); tags_buf.extend(e.tags()); bb.add_way(id, &tags_buf, ...)`
→ becomes `bb.add_way(id, e.tags(), ...)`  (no tags_buf needed at all!)

**Pattern B (elements_pbf):** `let tags: Vec<_> = owned.tags.iter().map(...).collect(); bb.add_way(id, &tags, ...)`
→ becomes `bb.add_way(id, owned.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), ...)`

**Pattern C (merge raw):** `bb.add_way_raw(id, &raw_keys, &raw_vals, ...)`
→ unchanged (raw methods use pre-encoded indices, not tags)

Pattern A is the biggest simplification — the `tags_buf` scratch Vec
becomes unnecessary for most callers. The API change actually reduces
code across the codebase.
