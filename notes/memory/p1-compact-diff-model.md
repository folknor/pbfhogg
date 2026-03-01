# P1: Compact Diff Model for Planet-Scale Memory Density

## Problem Statement

Planet-scale merge with 7-30 coalesced daily diffs is RSS-bound on the `DiffOverlay`.
A single planet daily diff contains ~4M changes. At 30 diffs that is up to ~120M
changes (with overlap -- realistic coalesced size ~20-40M unique entities).

The current representation uses `HashMap<i64, OscNode/Way/Relation>` with owned
`String` and `Vec` fields. At planet scale this dominates peak RSS, making 64 GB
machines tight for backlog catch-up scenarios.

**Target:** >=25% peak RSS reduction with <=10% time regression.

---

## Current State Analysis

### Data Types (`src/osc.rs`, lines 24-58)

```rust
pub struct OscNode {        // 56 bytes (on stack)
    pub id: i64,            //  8 bytes
    pub lat: f64,           //  8 bytes (wasteful: f64 stores ~15 decimal digits,
    pub lon: f64,           //  8 bytes  we only need 7 for decimicrodegrees)
    pub tags: Vec<(String, String)>,  // 24 bytes (ptr+len+cap) + heap
}

pub struct OscWay {         // 56 bytes (on stack)
    pub id: i64,            //  8 bytes
    pub node_refs: Vec<i64>,          // 24 bytes (ptr+len+cap) + heap
    pub tags: Vec<(String, String)>,  // 24 bytes (ptr+len+cap) + heap
}

pub struct OscRelMember {   // 40 bytes (on stack)
    pub member_type: MemberType,      //  8 bytes (i32 enum, padded)
    pub ref_id: i64,                  //  8 bytes
    pub role: String,                 // 24 bytes (ptr+len+cap) + heap
}

pub struct OscRelation {    // 56 bytes (on stack)
    pub id: i64,            //  8 bytes
    pub members: Vec<OscRelMember>,   // 24 bytes (ptr+len+cap) + heap
    pub tags: Vec<(String, String)>,  // 24 bytes (ptr+len+cap) + heap
}

pub struct DiffOverlay {
    pub nodes: HashMap<i64, OscNode>,
    pub ways: HashMap<i64, OscWay>,
    pub relations: HashMap<i64, OscRelation>,
    pub deleted_nodes: HashSet<i64>,
    pub deleted_ways: HashSet<i64>,
    pub deleted_relations: HashSet<i64>,
}
```

### Consumption Patterns (`src/commands/merge.rs`)

The overlay is consumed in these patterns:

1. **`DiffRanges::from_diff`** (line 151): iterates `.keys()` and `.deleted_*.iter()`
   to build sorted `Vec<i64>` for fast range overlap checks. Read-only on IDs.

2. **`block_overlaps_diff`** (line 336): `diff.deleted_*.contains(&id)` and
   `diff.*.contains_key(&id)` -- pure existence checks.

3. **`rewrite_block_parallel`** (line 654): For each element in a block, does
   `diff.deleted_*.contains(&id)` then `diff.*.get(&id)`. On hit, reads:
   - **Node:** `osc.id`, `osc.lat`, `osc.lon`, `osc.tags` (as `&[(&str, &str)]`)
   - **Way:** `osc.id`, `osc.tags` (as `&[(&str, &str)]`), `osc.node_refs` (as `&[i64]`)
   - **Relation:** `osc.id`, `osc.tags` (as `&[(&str, &str)]`), `osc.members` (iterated
     to build `Vec<MemberData<'_>>` with `m.ref_id`, `m.member_type`, `&m.role`)

4. **`emit_create_local`** / **`emit_create_for_output`** (lines 599, 891):
   Same access pattern as #3, triggered by upsert cursor.

5. **`write_osc_way`** / **`write_osc_relation`** (lines 444, 455):
   Same access -- tags as `(&str, &str)` slices, node_refs as `&[i64]`, members iterated.

6. **`DiffOverlay::merge`** (line 86): merges two overlays. Keys/contains checks + extend.

**Key observation:** All consumption converts to `&str` slices and `&[i64]` slices
before passing to `BlockBuilder`. The owned `String`/`Vec` representation is never
needed at the API boundary -- only borrowing is required.

### HashMap Overhead

std `HashMap<i64, T>` per entry:
- Hash bucket: 8 bytes (pointer in bucket array)
- Control byte: 1 byte (Swiss table metadata)
- Key: 8 bytes (i64)
- Value: sizeof(T) bytes
- Alignment padding
- Load factor ~87.5% (Swiss table), so ~14% wasted slots
- Total overhead per entry: ~17-25 bytes beyond the value itself

std `HashSet<i64>` per entry:
- Similar to HashMap<i64, ()>: ~17-25 bytes per entry

---

## Memory Cost Model

### Current Cost Per Entity (estimated)

**OscNode:**
- HashMap entry overhead: ~25 bytes
- OscNode struct: 56 bytes
- Tags heap: 24 bytes/tag-pair (two Strings: 2x(24 stack + 8-20 heap for short strings))
  Average OSM node has ~2 tags, each key ~8 chars, value ~12 chars.
  Per tag: 24+8 + 24+12 = 68 bytes heap + 48 bytes in Vec slot = 116 bytes.
  2 tags: 232 bytes + 24 (Vec header) = 256 bytes tag overhead.
  But most changed nodes have 0-1 tags. Estimate: 0.5 tags avg = ~82 bytes.
- **Total per node: ~163 bytes**

**OscWay:**
- HashMap entry overhead: ~25 bytes
- OscWay struct: 56 bytes
- node_refs heap: avg way has ~8 refs. 8 * 8 = 64 bytes + Vec overhead = ~88 bytes.
  But changed ways often have more refs. Estimate ~12 refs avg = ~120 bytes.
- Tags heap: avg ~3 tags. 3 * 116 = 348 bytes + 24 = ~372 bytes.
- **Total per way: ~573 bytes**

**OscRelation:**
- HashMap entry overhead: ~25 bytes
- OscRelation struct: 56 bytes
- Members heap: avg ~5 members. Each OscRelMember is 40 bytes + role String heap (~16 bytes).
  5 * (40 + 16) = 280 bytes + 24 = ~304 bytes.
- Tags heap: avg ~4 tags. 4 * 116 = 464 + 24 = ~488 bytes.
- **Total per relation: ~873 bytes**

**Delete sets:**
- Per deleted entity: ~25 bytes (HashSet<i64> entry)

### Planet-Scale Backlog Estimate (30 daily diffs coalesced)

Rough estimates for 30-day backlog (unique entities after coalescing):
- ~12M nodes, ~3M ways, ~200K relations, ~2M deletes total

| Component | Count | Bytes/entity | Total |
|-----------|-------|-------------|-------|
| Nodes | 12M | 163 | 1.86 GB |
| Ways | 3M | 573 | 1.64 GB |
| Relations | 200K | 873 | 166 MB |
| Delete sets | 2M | 25 | 48 MB |
| DiffRanges | 17M IDs | 8 | 130 MB |
| HashMap bucket arrays | - | - | ~200 MB |
| **Total** | | | **~4.0 GB** |

For a single daily diff (~4M changes: ~3M nodes, ~800K ways, ~200K rels):

| Component | Count | Bytes/entity | Total |
|-----------|-------|-------------|-------|
| Nodes | 3M | 163 | 465 MB |
| Ways | 800K | 573 | 437 MB |
| Relations | 200K | 873 | 166 MB |
| Delete sets | 500K | 25 | 12 MB |
| **Total** | | | **~1.1 GB** |

---

## Proposed Design: Compact Arena-Based Overlay

### Core Idea

Replace per-entity heap allocations with three flat arenas (one per entity type)
plus `HashMap<i64, u32>` indexes that map entity IDs to arena offsets. Intern
tag keys globally (there are only ~50K unique OSM tag keys worldwide). Store
coordinates as `i32` decimicrodegrees.

### Data Layout

```rust
/// A contiguous byte arena for variable-length entity data.
/// Entities are appended sequentially. Each entity is prefixed with
/// a fixed-size header, followed by variable-length tag/ref/member data.
struct EntityArena {
    /// The contiguous buffer holding all entity data.
    data: Vec<u8>,
}

/// Global string interner for tag keys and relation member roles.
/// Tag values are NOT interned (too many unique values, poor hit rate).
struct StringInterner {
    /// Deduplicated strings stored contiguously.
    strings: Vec<u8>,
    /// Offset table: intern_id -> (offset, len) in `strings`.
    offsets: Vec<(u32, u16)>,
    /// Lookup: string bytes -> intern_id.
    index: FxHashMap<u32, u32>,  // hash of string -> intern_id (with collision chain)
}

/// Compact node: 16 bytes fixed (no per-node heap alloc).
///   id:  i64 (8 bytes)
///   lat: i32 (4 bytes, decimicrodegrees)
///   lon: i32 (4 bytes, decimicrodegrees)
///   Followed by: tag_count: u16, then tag_count pairs of (key_intern_id: u32, value_offset: u32, value_len: u16).
///   Tag values stored inline in the arena after the tag index entries.

/// Compact way: 16 bytes fixed header.
///   id:       i64 (8 bytes)
///   ref_count: u32 (4 bytes)
///   tag_count: u16 (2 bytes)
///   _pad:      u16 (2 bytes)
///   Followed by: ref_count i64 values (8 bytes each), then tag data (same format as node).

/// Compact relation: 16 bytes fixed header.
///   id:           i64 (8 bytes)
///   member_count: u32 (4 bytes)
///   tag_count:    u16 (2 bytes)
///   _pad:         u16 (2 bytes)
///   Followed by: member_count entries of (ref_id: i64, type: u8, role_intern_id: u32) = 13 bytes each,
///   then tag data.

pub struct CompactDiffOverlay {
    // --- Entity arenas ---
    node_arena: Vec<u8>,
    way_arena: Vec<u8>,
    relation_arena: Vec<u8>,

    // --- ID -> arena offset indexes ---
    node_index: FxHashMap<i64, u32>,
    way_index: FxHashMap<i64, u32>,
    relation_index: FxHashMap<i64, u32>,

    // --- Delete sets (unchanged, already efficient) ---
    deleted_nodes: HashSet<i64>,
    deleted_ways: HashSet<i64>,
    deleted_relations: HashSet<i64>,

    // --- String interning ---
    interner: StringInterner,
}
```

### Arena Wire Format

Each entity is stored as a packed byte sequence in its arena. No alignment
requirements (read via `from_le_bytes` on copied byte arrays, not pointer casts).

**Node layout (variable length, typically 20-40 bytes):**
```
[id: i64 LE][lat: i32 LE][lon: i32 LE][tag_count: u16 LE]
  for each tag:
    [key_intern_id: u32 LE][value_len: u16 LE][value_bytes: value_len bytes]
```

**Way layout (variable length):**
```
[id: i64 LE][ref_count: u32 LE][tag_count: u16 LE][pad: u16]
  [ref_0: i64 LE] ... [ref_N: i64 LE]        (ref_count entries)
  for each tag:
    [key_intern_id: u32 LE][value_len: u16 LE][value_bytes: value_len bytes]
```

**Relation layout (variable length):**
```
[id: i64 LE][member_count: u32 LE][tag_count: u16 LE][pad: u16]
  for each member:
    [ref_id: i64 LE][type: u8][role_intern_id: u32 LE]    (13 bytes each)
  for each tag:
    [key_intern_id: u32 LE][value_len: u16 LE][value_bytes: value_len bytes]
```

### Why This Layout

1. **No per-entity heap allocations.** Every `String`, `Vec<(String, String)>`,
   `Vec<i64>`, `Vec<OscRelMember>` is eliminated. All data lives in the arena.

2. **Coordinates as i32.** `f64` uses 8 bytes for ~15 significant digits.
   Decimicrodegrees (10^-7 degrees, ~1cm resolution) fits in `i32` (4 bytes).
   The merge code already calls `to_decimicro()` on every node -- we do that
   conversion once at parse time instead.

3. **String interning for keys and roles.** OSM tag keys are highly repetitive
   (~50K unique keys globally, most entities use the same ~200 keys). Interning
   replaces a 24-byte `String` + heap allocation with a 4-byte `u32` intern ID.
   Relation member roles are similarly repetitive ("inner", "outer", "stop",
   "platform", "" -- maybe ~500 unique values). Both are interned.

4. **Tag values NOT interned.** Tag values have much higher cardinality (street
   names, phone numbers, URLs). Interning them would bloat the interner's hash
   table with poor hit rates. Values are stored inline in the arena.

5. **FxHashMap<i64, u32> instead of HashMap<i64, OscEntity>.** The value shrinks
   from 56 bytes (OscNode/Way/Relation struct) to 4 bytes (arena offset). This
   reduces HashMap memory by ~80% per slot. FxHashMap further saves ~15% vs std
   HashMap for integer keys.

### Accessor API

The overlay provides zero-copy accessor types that borrow from the arena:

```rust
/// Zero-copy view into a node stored in the arena.
pub struct CompactNodeRef<'a> {
    data: &'a [u8],  // slice into node_arena
    interner: &'a StringInterner,
}

impl<'a> CompactNodeRef<'a> {
    pub fn id(&self) -> i64 { ... }
    pub fn decimicro_lat(&self) -> i32 { ... }
    pub fn decimicro_lon(&self) -> i32 { ... }
    pub fn lat(&self) -> f64 { self.decimicro_lat() as f64 * 1e-7 }
    pub fn lon(&self) -> f64 { self.decimicro_lon() as f64 * 1e-7 }
    pub fn tag_count(&self) -> usize { ... }
    /// Iterate tags as (&str, &str) pairs. Keys resolved via interner.
    pub fn tags(&self) -> impl Iterator<Item = (&'a str, &'a str)> { ... }
}

// Similar for CompactWayRef, CompactRelationRef
```

### Proposed Cost Per Entity

**CompactNode:**
- FxHashMap entry overhead: ~20 bytes (key i64 + value u32 + control)
- Arena: 8 (id) + 4 (lat) + 4 (lon) + 2 (tag_count) = 18 bytes fixed
  + per tag: 4 (key intern) + 2 (val_len) + avg 12 (val bytes) = 18 bytes
  0.5 tags avg: 9 bytes
- **Total per node: ~47 bytes** (vs 163 current, **-71%**)

**CompactWay:**
- FxHashMap entry: ~20 bytes
- Arena: 8 (id) + 4 (ref_count) + 2 (tag_count) + 2 (pad) = 16 bytes fixed
  + 12 refs * 8 = 96 bytes
  + 3 tags * 18 = 54 bytes
- **Total per way: ~186 bytes** (vs 573 current, **-68%**)

**CompactRelation:**
- FxHashMap entry: ~20 bytes
- Arena: 8 (id) + 4 (member_count) + 2 (tag_count) + 2 (pad) = 16 bytes fixed
  + 5 members * 13 = 65 bytes
  + 4 tags * 18 = 72 bytes
- **Total per relation: ~173 bytes** (vs 873 current, **-80%**)

### Planet-Scale Savings Estimate (30-day backlog)

| Component | Current | Proposed | Savings |
|-----------|---------|----------|---------|
| 12M nodes | 1.86 GB | 540 MB | -71% |
| 3M ways | 1.64 GB | 533 MB | -68% |
| 200K relations | 166 MB | 33 MB | -80% |
| Delete sets | 48 MB | 48 MB | 0% |
| DiffRanges | 130 MB | 130 MB | 0% |
| HashMap overhead | 200 MB | 80 MB | -60% |
| String interner | 0 | ~2 MB | +2 MB |
| **Total** | **~4.0 GB** | **~1.37 GB** | **-66%** |

For a single daily diff (~4M changes):

| Component | Current | Proposed | Savings |
|-----------|---------|----------|---------|
| Total overlay | ~1.1 GB | ~380 MB | **-65%** |

This comfortably exceeds the 25% target.

---

## String Interning Strategy

### What to Intern

| String type | Unique count (planet) | Frequency | Intern? | Rationale |
|------------|----------------------|-----------|---------|-----------|
| Tag keys | ~50K | Very high | **Yes** | Same 200 keys on 95% of entities |
| Member roles | ~500 | Very high | **Yes** | "inner", "outer", "", "stop", etc. |
| Tag values | ~100M+ | Very low | **No** | Street names, numbers, URLs -- poor reuse |

### Interner Design

```rust
pub struct StringInterner {
    /// Contiguous buffer of all interned strings (no per-string heap alloc).
    data: Vec<u8>,
    /// Offset table: intern_id -> (start_offset, length).
    /// intern_id 0 is reserved for the empty string.
    table: Vec<(u32, u16)>,
    /// Reverse lookup for dedup: FxHashMap from hash(bytes) to intern_id.
    /// On collision, linear probe through table entries comparing bytes.
    lookup: FxHashMap<u64, u32>,
}

impl StringInterner {
    /// Intern a string, returning its ID. Deduplicates.
    fn intern(&mut self, s: &str) -> u32 { ... }
    /// Resolve an intern ID back to &str.
    fn resolve(&self, id: u32) -> &str { ... }
}
```

Memory cost of interner for planet-scale:
- ~50K unique tag keys, avg 12 bytes: ~600 KB in `data`
- ~500 unique roles, avg 6 bytes: ~3 KB in `data`
- ~50.5K entries in `table`: ~300 KB
- ~50.5K entries in `lookup`: ~600 KB
- **Total: ~1.5 MB** (negligible)

---

## Implementation Plan

### Phase 1: Core Data Structures (src/osc.rs)

1. Add `StringInterner` struct (new private type in `src/osc.rs` or a small
   `src/string_intern.rs` module).

2. Add `CompactDiffOverlay` struct with arena `Vec<u8>` buffers and
   `FxHashMap<i64, u32>` indexes.

3. Add arena append methods:
   - `append_node(id, lat_i32, lon_i32, tags_iter) -> u32` (returns offset)
   - `append_way(id, refs_iter, tags_iter) -> u32`
   - `append_relation(id, members_iter, tags_iter) -> u32`

4. Add zero-copy accessor types (`CompactNodeRef`, `CompactWayRef`,
   `CompactRelationRef`) with methods matching the consumption patterns in merge.rs.

5. Add `CompactDiffOverlay::merge()` -- merges two overlays. For the "later wins"
   semantics, the winning entity's arena bytes are appended to the target arena
   and the index is updated. The losing entity's bytes become dead space in the
   arena (acceptable fragmentation -- at most 2x arena size in worst case, and
   typical merge only overwrites ~5% of entities).

6. Convert `parse_osc_file()` to build `CompactDiffOverlay` directly:
   - Parse lat/lon as f64, immediately convert to i32 decimicro
   - Intern tag keys and member roles during parse
   - Append tag values inline
   - Write entity bytes to arena, insert offset into index

### Phase 2: Migrate merge.rs Consumption

The merge.rs consumption changes are mechanical. Each access pattern maps cleanly:

| Current pattern | New pattern |
|----------------|-------------|
| `diff.nodes.get(&id)` -> `Some(&OscNode)` | `diff.get_node(id)` -> `Option<CompactNodeRef<'_>>` |
| `diff.nodes.contains_key(&id)` | `diff.node_index.contains_key(&id)` |
| `diff.deleted_nodes.contains(&id)` | `diff.deleted_nodes.contains(&id)` (unchanged) |
| `diff.nodes.keys()` | `diff.node_index.keys()` |
| `osc.tags.iter().map(\|(k,v)\| (k.as_str(), v.as_str())).collect()` | `node_ref.tags().collect::<Vec<_>>()` |
| `osc.node_refs` as `&[i64]` | `way_ref.refs().collect::<Vec<_>>()` or directly iterate |
| `osc.members.iter().map(...)` | `rel_ref.members().map(...)` |

**Critical change in `rewrite_block_parallel`:** The `diff: &DiffOverlay` parameter
becomes `diff: &CompactDiffOverlay`. The accessor references borrow from the overlay,
which is `&`-shared across rayon workers (read-only during merge). This is safe because
the overlay is built before merge begins and never mutated during the merge pass.

### Phase 3: Coordinate Conversion Elimination

Currently `to_decimicro(osc.lat)` is called at every node write site in merge.rs
(lines 613, 695-698, 716-719, 905). With i32 storage, the conversion happens once
at parse time. The merge code uses the pre-converted value directly.

This is a minor but clean performance win: eliminates `(deg * 1e7).round() as i32`
per node during the hot rewrite path.

### Phase 4: DiffRanges Optimization

`DiffRanges::from_diff()` currently collects `diff.nodes.keys()` into a `Vec<i64>`,
sorts, and dedups. With `FxHashMap<i64, u32>`, the `.keys()` iterator works
identically. No change needed, but we could explore using the arena's sequential
layout to extract sorted IDs more efficiently (nodes are inserted in XML order,
which is roughly sorted).

### Phase 5: Delete Set Optimization (optional, stretch)

The `HashSet<i64>` delete sets are already compact (~25 bytes/entry). Could be
replaced with sorted `Vec<i64>` + binary search (8 bytes/entry, better cache
behavior). However, deletes are a small fraction of changes (~10%), so the
absolute savings are modest (~10 MB at 30-day planet backlog). Defer unless
easy to add.

---

## Detailed File Changes

### `src/osc.rs`

1. **Keep** `OscNode`, `OscWay`, `OscRelMember`, `OscRelation`, `DiffOverlay`
   for backward compatibility (the module is `pub`). Mark them `#[deprecated]`
   if desired, or keep as-is since they are only used internally by merge.

2. **Add** `StringInterner` (~80 lines).

3. **Add** `CompactDiffOverlay` struct + impl (~200 lines):
   - `new()`, `is_empty()`, `merge()`, `get_node()`, `get_way()`, `get_relation()`
   - `has_node()`, `has_way()`, `has_relation()` for contains_key checks
   - `node_ids()`, `way_ids()`, `relation_ids()` for key iteration

4. **Add** `CompactNodeRef`, `CompactWayRef`, `CompactRelationRef` (~150 lines):
   - Zero-copy accessors with `id()`, `tags()`, `refs()`, `members()` etc.

5. **Modify** `parse_osc_file()` to return `CompactDiffOverlay`:
   - Change return type from `ParseResult<DiffOverlay>` to `ParseResult<CompactDiffOverlay>`
   - Build entities in arena format during parse
   - Intern tag keys and roles

6. **Modify** `load_all_diffs()` to use `CompactDiffOverlay::merge()`.

7. **Update tests** to use new accessor API.

### `src/commands/merge.rs`

1. **Change imports** (line 29): `DiffOverlay` -> `CompactDiffOverlay`, remove
   `OscRelMember`, `OscRelation`, `OscWay`.

2. **Update `DiffRanges::from_diff`** (line 151): use `.node_ids()` etc.

3. **Update `block_overlaps_diff`** (line 336): use `.has_node()`, `.has_way()` etc.

4. **Update `emit_create_local`** (line 599): use `diff.get_node(id)` returning
   `CompactNodeRef`, call `.decimicro_lat()` / `.decimicro_lon()` directly
   (no `to_decimicro()` call), collect tags via `.tags()`.

5. **Update `rewrite_block_parallel`** (line 654): same pattern changes.

6. **Update `write_osc_way`** / `write_osc_relation`** (lines 444, 455): adapt
   to `CompactWayRef` / `CompactRelationRef` accessors.

7. **Update `emit_create_for_output`** (line 891): same pattern.

8. **Update `merge()` entry** (line 943): `parse_osc_file` now returns
   `CompactDiffOverlay`.

### `Cargo.toml`

No new dependencies needed. `rustc-hash` is already available for `FxHashMap`.
The arena is just `Vec<u8>` -- no arena crate needed. The string interner is
hand-rolled (~80 lines) to avoid adding a dependency for such a simple structure.

---

## Risk Assessment

### Low Risk

- **Correctness:** The arena stores the exact same data, just packed differently.
  Round-trip tests (`roundtrip.rs`, `roundtrip_real.rs`) and `brokkr verify merge`
  will catch any encoding errors.

- **API stability:** `DiffOverlay` is `pub` but only consumed by `merge.rs` internally.
  No downstream crate depends on the `Osc*` types. The `osc` module is public but
  the types are not part of any stable API contract.

- **Performance regression:** Arena append is a single `Vec::extend_from_slice`,
  faster than individual `String`/`Vec` allocations. HashMap lookups are slightly
  faster (smaller values = better cache density). The only potential slowdown is
  the extra byte-unpacking in the accessor methods, but this is offset by
  eliminating `to_decimicro()` and `collect::<Vec<_>>()` allocations at each
  write site.

### Medium Risk

- **Arena fragmentation during merge:** When `CompactDiffOverlay::merge()` overwrites
  an entity, the old arena bytes become dead space. For 30-diff coalescing with ~5%
  overwrites per diff, total dead space is ~15% of arena size. This is acceptable
  and can be compacted if needed (but likely not worth the complexity).

- **Tag value length > 65535 bytes:** Using `u16` for value length limits values to
  64 KB. OSM wiki says max tag value is 255 characters, but some values (notably
  `description` and `note`) can be longer. Mitigation: use `u16` with a sentinel
  value (0xFFFF) that triggers a 4-byte length fallback read. Or just use `u32`
  (adds 2 bytes per tag, still much cheaper than `String`).

### Rollback Strategy

The old `DiffOverlay` types remain in the codebase (can be gated behind
`#[cfg(test)]` or kept for reference). If the compact overlay causes issues:

1. Revert `parse_osc_file()` return type to `DiffOverlay`.
2. Revert merge.rs imports and access patterns.
3. All changes are in two files (`osc.rs`, `merge.rs`) -- easy to revert as a
   single commit.

---

## Testing Strategy

### Correctness

1. **Unit tests in `osc.rs`:** Adapt existing tests to use `CompactDiffOverlay`.
   Add round-trip tests: parse -> arena -> accessor -> verify all fields match.

2. **`brokkr verify merge`:** Cross-validates merge output against osmium, osmosis,
   and osmconvert. Must pass identically before and after the change.

3. **`brokkr check -- --ignored`:** Runs `roundtrip_denmark` which exercises the
   full read-write-read cycle. Must pass.

4. **Add a specific test** for merge with CompactDiffOverlay: parse a known .osc.gz,
   verify exact entity counts, tag contents, and coordinate values match.

5. **Merge coalescing test:** Load 3+ diffs via `load_all_diffs`, verify that
   later-wins semantics, delete-removes-create, and create-removes-delete all
   work correctly with the compact representation.

### Memory Measurement

1. **Before/after RSS:** Run `brokkr bench merge --dataset germany` and measure
   peak RSS via `/proc/self/status` VmHWM or `getrusage(RUSAGE_SELF)`.

2. **Before/after RSS for multi-diff:** Create a test harness that loads N diffs
   (simulate planet backlog) and reports overlay memory via a `memory_usage()`
   method on `CompactDiffOverlay` that sums arena sizes + HashMap capacities.

3. **Valgrind/DHAT:** Optional deep analysis to verify allocation reduction.

### Performance Regression

1. **`brokkr bench merge --dataset germany`:** Compare wall-clock before and after.
   Must be within 10% of baseline. Expected: roughly equal or slightly faster
   (fewer allocations, better cache behavior).

2. **`brokkr bench merge --dataset denmark`:** Sanity check on smaller dataset.

---

## Implementation Effort Estimate

| Task | Effort | Lines Changed |
|------|--------|--------------|
| StringInterner implementation | 1 hour | ~80 new |
| CompactDiffOverlay + arena format | 3 hours | ~250 new |
| Accessor types (NodeRef, WayRef, RelRef) | 2 hours | ~150 new |
| Migrate parse_osc_file() | 2 hours | ~100 modified |
| Migrate load_all_diffs() + merge() | 1 hour | ~30 modified |
| Migrate merge.rs consumption | 3 hours | ~200 modified |
| Update osc.rs tests | 1 hour | ~80 modified |
| Benchmarking + verification | 2 hours | - |
| **Total** | **~15 hours** | ~600 new + 400 modified |

The change is entirely contained in `src/osc.rs` and `src/commands/merge.rs`.
No other files in the codebase reference the `Osc*` types or `DiffOverlay`.

---

## Open Questions

1. **Should we keep the old `DiffOverlay` as a public type?** It is `pub` but has
   no known external consumers. Could deprecate or remove. Recommend: remove the
   old types entirely since they are not part of a stable API.

2. **Tag value length encoding:** `u16` (64 KB max) vs `u32` (wastes 2 bytes/tag).
   Recommend `u16` -- OSM values are practically always < 1 KB. Add a
   `debug_assert!(value.len() <= u16::MAX as usize)` with a graceful truncation
   fallback.

3. **Arena compaction after merge coalescing:** Is it worth adding a `compact()`
   method to reclaim dead space? At ~15% dead space on 30-diff coalescing, a
   1.37 GB overlay wastes ~200 MB. Probably not worth the complexity for V1.
   Can add later if profiling shows it matters.

4. **Should `deleted_*` sets use sorted `Vec<i64>` + binary search?** Saves ~60%
   per delete entry but adds O(n log n) sort cost. Deletes are ~10% of changes.
   Defer to a separate optimization if delete sets are shown to matter.
