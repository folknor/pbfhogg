# Consolidation Review #2: Shared ElementEmitter API

## Verdict: DO NOT DO

## Complete Inventory of Emission Sites

### Category A: "Standard Pattern" (flush_local + tags/refs/members buffers + metadata)

Canonical pattern:
```rust
if !bb.can_add_<type>() {
    flush_local(bb, output)?;
}
tags_buf.clear();
tags_buf.extend(<element>.tags());
// [refs_buf / members_buf for ways/relations]
let meta = dense_node_metadata(dn);  // or element_metadata(&n.info())
bb.add_<type>(..., &tags_buf, meta.as_ref());
```

Sites using this exact pattern:

| File | Function | DenseNode | Node | Way | Relation | Total |
|---|---|---|---|---|---|---|
| `cat.rs` | `process_block` | 1 | 1 | 1 | 1 | **4** |
| `getid.rs` | `process_block` | 1 | 1 | 1 | 1 | **4** |
| `tags_filter.rs` | `filter_block_parallel` | 1 | 1 | 1 | 1 | **4** |
| `tags_filter.rs` | `filter_block_pass2` | 1 | 1 | 1 | 1 | **4** |
| `extract.rs` | `extract_block_pass2` | 1 | 1 | 1 | 1 | **4** |
| `extract.rs` | `extract_block_pass3` | 1 | 1 | 1 | 1 | **4** |
| `add_locations_to_ways.rs` | `process_block` | 1 | 1 | 1* | 1 | **4** |

*The way emission in `add_locations_to_ways` uses `bb.add_way_with_locations()` instead of `bb.add_way()` -- a **unique variant**.

**Subtotal: 28 emission sites (7 functions x 4 element types)**

### Category B: "Standard Pattern" with `flush_block` (flushing to `PbfWriter`)

| File | Function | Total |
|---|---|---|
| `sort.rs` | `write_single_node` | **1** |
| `sort.rs` | `write_single_way` | **1** |
| `sort.rs` | `write_single_relation` | **1** |

Sort uses **owned data** with `owned_to_metadata()` conversion. No buffer reuse.

**Subtotal: 3 emission sites**

### Category C: Merge -- OSC element emission (no metadata)

| File | Function | Pattern | Count |
|---|---|---|---|
| `merge.rs` | `emit_create_local` | node/way/relation from OSC, metadata=`None` | **3** |
| `merge.rs` | `rewrite_block_parallel` | 4 element arms with modify+create paths | ~**8** |
| `merge.rs` | `emit_create_for_output` | node/way/relation from OSC, `flush_block` | **3** |
| `merge.rs` | `write_osc_way` / `write_osc_relation` | OSC elements via `flush_block` | **2** |

**Subtotal: ~16 emission sites**

### Category D: Merge -- Raw passthrough (pre-seeded string table)

| File | Function | API | Count |
|---|---|---|---|
| `merge.rs` | `write_base_dense_node_local` | `bb.add_node_raw()` with `RawMetadata` | **1** |
| `merge.rs` | `write_base_node_local` | `bb.add_node_raw()` with `RawMetadata` | **1** |
| `merge.rs` | `write_base_way_local` | `bb.add_way_raw_bytes()` with raw wire data | **1** |
| `merge.rs` | `write_base_relation_local` | `bb.add_relation_raw_bytes()` with raw wire data | **1** |

Uses pre-seeded string tables and raw wire bytes. Completely incompatible with the standard path.

**Subtotal: 4 emission sites (fundamentally different API)**

### Category E: Merge -- Capacity helpers (already extracted)

Merge has already factored out the capacity-check pattern into 6 helpers:
```rust
fn ensure_node_capacity(bb, writer) -> Result<()>       // flush_block
fn ensure_way_capacity(bb, writer) -> Result<()>
fn ensure_relation_capacity(bb, writer) -> Result<()>
fn ensure_node_capacity_local(bb, output) -> Result<()>  // flush_local
fn ensure_way_capacity_local(bb, output) -> Result<()>
fn ensure_relation_capacity_local(bb, output) -> Result<()>
```

This is itself an attempt at the exact abstraction being proposed -- and it stopped there because the full element emission diverges too much.

## Metadata Mode Analysis

Four distinct metadata modes:

| Mode | Used by | API |
|---|---|---|
| `Metadata<'a>` via helpers | cat, getid, tags_filter, extract, add-locations, sort | `bb.add_node(..., meta.as_ref())` |
| `None` (no metadata) | merge OSC elements | `bb.add_node(..., None)` |
| `RawMetadata` | merge base passthrough | `bb.add_node_raw(..., meta.as_ref())` |
| `owned_to_metadata()` | sort overlap runs | `bb.add_node(..., meta.as_ref())` |

## Buffer Reuse Analysis

**Current state:** Each `process_block` function declares local buffers:
```rust
let mut tags_buf: Vec<(&str, &str)> = Vec::new();
let mut refs_buf: Vec<i64> = Vec::new();
let mut members_buf: Vec<MemberData<'_>> = Vec::new();
```

Created per `process_block` call on rayon threads via `map_init`. Within a call, they are reused across elements (clear + extend). **This is already correct and efficient.**

Only `add_locations_to_ways` correctly hoists `refs_buf` and `locations_buf` into the `map_init` init closure for cross-batch reuse.

## Flush Destination Analysis

Two flush destinations:

1. **`flush_local(bb, output)` -> `Vec<OwnedBlock>`**: Used by all parallel batch processing.
2. **`flush_block(bb, writer)` -> `PbfWriter`**: Used by sequential write paths (sort, merge gap/trailing).

An `ElementEmitter` would need to be generic over both.

## What Would an ElementEmitter Look Like?

```rust
struct ElementEmitter<'a> {
    bb: &'a mut BlockBuilder,
    output: &'a mut Vec<OwnedBlock>,
    tags_buf: Vec<(&'a str, &'a str)>,   // PROBLEM: lifetime 'a is wrong
    refs_buf: Vec<i64>,
    members_buf: Vec<MemberData<'a>>,    // PROBLEM: lifetime 'a is wrong
}
```

**Critical problem: lifetime incompatibility.** The tag buffers borrow from the `Element`, which is iteration-scoped. A struct that owns these buffers cannot hold references across iterations without unsafe code.

A function-based approach avoids this but only saves ~4 lines per call site:

```rust
fn emit_dense_node(
    dn: &DenseNode<'_>,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    tags_buf: &mut Vec<(&str, &str)>,
) -> Result<(), String> {
    if !bb.can_add_node() { flush_local(bb, output)?; }
    tags_buf.clear();
    tags_buf.extend(dn.tags());
    let meta = dense_node_metadata(dn);
    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), tags_buf, meta.as_ref());
    Ok(())
}
```

Total savings on 28 sites: ~112 lines, minus ~60 lines of helper definitions, netting **~50 lines**.

## What CANNOT Be Unified

1. **Merge raw passthrough** (`add_node_raw`, `add_way_raw_bytes`, `add_relation_raw_bytes`): Completely different API.
2. **`add_way_with_locations`** (add_locations_to_ways): Unique API.
3. **Sort overlap run** (`write_single_node/way/relation`): Operates on owned data.
4. **Merge OSC elements**: Data comes from `CompactDiffOverlay`, not `Element` enum.

## Performance Risk

**Negligible.** The capacity check fires once every ~8000 elements. `#[inline]` on small helpers would address any call overhead.

## Recommendation: DO NOT DO

1. **The unifiable sites share only ~12 lines of code per element type.** Concrete, readable, locally understandable.
2. **Lifetime complications make a struct-based emitter awkward.**
3. **The "variants" are genuinely diverse.** 4 metadata modes, 2 flush destinations, 7 different BlockBuilder APIs, 3 source types.
4. **Merge has already extracted the reusable part.** The `ensure_*_capacity` helpers are the right abstraction level.
5. **Buffer reuse is already adequate.**
6. **Net savings: ~50 lines** out of ~5000+ lines. Not worth the abstraction cost.

### What would be worth doing instead (small, concrete):

- **Promote `ensure_*_capacity_local` to `mod.rs`:** Saves 1 line per emission site (28 sites) with zero API complexity.
- **Hoist buffer Vecs into `map_init`:** In cat, getid, tags_filter, and extract, move `tags_buf`, `refs_buf`, `members_buf` into the `map_init` init closure for cross-batch reuse.
