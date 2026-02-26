# `rewrite_block` cost breakdown investigation

Analysis of merge's `rewrite_block` cost, covering both remaining merge optimization
theories from TODO.md: **pre-seed output StringTable** and **raw packed bytes for
non-string integer fields**.

Profiling data from `notes/hotpath-profile.md` (Denmark seq4704+seq4705, commit
d5c8095, fat LTO, zlib-ng). Code analysis only — no new profiling runs.

## Context

Denmark merge: 630 of 7396 blobs rewritten, ~4.4M elements re-encoded (2.6M nodes,
2.4M ways, 46K relations). The diff has ~9K changes out of 59.1M elements, so
>99.9% of elements in rewritten blocks are unmodified base elements being
round-tripped.

Merge hotpath timings (from `notes/hotpath-profile.md`):

| Function                | Calls     | Avg    | Total  | % Wall |
|-------------------------|-----------|--------|--------|--------|
| merge::rewrite_block    | 630       | 3.16ms | 1.99s  | 57%    |
| block_builder::add_way  | 2,408,901 | 286 ns | 690 ms | 20%    |
| block_builder::take     | 7,407     | 91 µs  | 676 ms | 19%    |
| block_builder::add_node | 2,573,619 | 48 ns  | 126 ms | 3.6%   |
| block_builder::add_rel  | 46,108    | 566 ns | 26 ms  | 0.7%   |

`rewrite_block` (1.99s) includes the add_* and some take calls within it.
The remaining ~470ms is read-side decode (element iteration, varint parsing,
diff lookups).

## Read side: essentially free

Element iteration and field access in `rewrite_block` are all O(1) per element
with zero allocation:

- `block.elements()`: lazy iterator, decodes one wire message per `.next()`
- `dn.tags()`: varint-decode string table indices, return `&str` slices directly
  from the decompressed buffer via `from_utf8_unchecked` (validated once at block
  construction)
- `dn.id()`, `decimicro_lat/lon()`: pre-decoded fields, inline arithmetic
- `way.refs()`: varint-decode delta-encoded i64s, O(1) per ref
- `rel.members()`: 3 varint reads per member, O(1)
- `element_metadata()` / `dense_node_metadata()`: O(1) field extraction

The read side contributes zero allocation and minimal CPU. All cost is on the
write side (BlockBuilder).

## StringTable::add — the dominant cost

### The allocation-on-every-call problem

`StringTable::add()` (`block_builder.rs:98-115`) calls `self.index.entry(s.to_owned())`
on every invocation. This allocates a `String` even when the string already exists
(Occupied path). On a cache hit, the allocated String is immediately discarded:

```rust
match self.index.entry(s.to_owned()) {     // ← allocates String
    Entry::Occupied(e) => *e.get(),         // ← String dropped, wasted
    Entry::Vacant(e) => { ... }
}
```

For a typical 8000-element dense-node block:
- ~99% of add() calls are cache hits (Occupied) — the String is wasted
- ~1% are new inserts (Vacant) — legitimate allocation

### Measured cost per add() call

**Profiled** with `#[hotpath::measure]` on `StringTable::add` (cat --type,
Denmark, full decode+write):

| Metric                 | Value         |
|------------------------|---------------|
| Total add() calls      | 131,080,697   |
| Average time           | 27ns          |
| Total time             | 3.55s (5.76%) |
| Calls per element      | 2.22          |

The 27ns average is the function body time. It covers:
1. `s.to_owned()`: malloc + memcpy for 4-15 byte strings
2. FxHash compute: multiply-rotate, 1-2 cycles per 8-byte chunk
3. HashMap probe: good locality for <256 entry table
4. String drop on Occupied hit (discarded immediately)

**Measurement overhead validation:** instrumenting add() inflates parent timings
because ~148ns hotpath overhead per call propagates upward:

| Function     | Before (uninstrumented) | After (add instrumented) | Delta  |
|--------------|-------------------------|--------------------------|--------|
| add_node     | 48ns                    | 285ns                    | +237ns |
| add_way      | 286ns                   | 767ns                    | +481ns |
| add_relation | 566ns                   | 2.86µs                   | +2.29µs|

The deltas are consistent with per-add() overhead × calls-per-element
(e.g. add_node: 237ns / 1.6 calls ≈ 148ns overhead per measurement).

### How many add() calls per element?

Measured: 131M calls / 59.1M elements = **2.22 calls/element** (global average).

Per element type (from call counts and element statistics):

**Dense node (add_node):**
- ~85% of nodes are tagless (way geometry points) → 0 tag add() calls
- ~15% have tags, averaging ~2 tags → 4 add() calls
- Metadata user string → 1 add() call (when metadata present)
- **Average: ~1.6 add() calls per node**

**Way (add_way):**
- Tags: ~3 tags average → 6 add() calls
- Refs: no string table work (i64 delta encoding)
- Metadata user: 1 add() call
- **Average: ~7 add() calls per way**

**Relation (add_relation):**
- Tags: ~3 tags → 6 add() calls
- Member roles: ~10 members → 10 add() calls
- Metadata user: 1 add() call
- **Average: ~17 add() calls per relation**

### Bottom-up cost model (confirmed by profiling)

| Function    | add() calls/elem | × 27ns | Predicted | Measured | StringTable % |
|-------------|-------------------|--------|-----------|----------|---------------|
| add_node    | 1.6               | 43ns   | ~43ns     | 48ns     | ~90%          |
| add_way     | 7                 | 189ns  | ~189ns    | 286ns    | ~66%          |
| add_relation| 17                | 459ns  | ~459ns    | 566ns    | ~81%          |

The model accounts for 66-90% of each add_* function's measured time. The
remaining cost is delta encoding, Vec pushes, and proto struct construction.

**Total StringTable::add as % of add_* combined:** 3.55s / 4.44s = **80%**.
This confirms StringTable::add dominates BlockBuilder insertion cost.

### Total StringTable cost in rewrite_block

Scaling from the profiled 80% ratio:

- From add_node: 126ms × 80% = ~101ms
- From add_way: 690ms × 80% = ~552ms
- From add_relation: 26ms × 80% = ~21ms
- **Total: ~674ms of rewrite_block's 1.99s = 34%**

Allocation churn: ~11.1M add() calls (5M elements × 2.22) × ~15 bytes avg
string = ~167 MB of immediately-discarded String allocations in merge's
rewrite path alone.

## TODO item: Pre-seed output StringTable

### The theory

From TODO.md: "The real win would be avoiding the `add()` call entirely for
unmodified elements by preserving input string table indices in the output."

For the >99.9% of elements that are unmodified, their tag keys, tag values, user
strings, and role strings already exist in the input block's string table. If the
output block used the same string table (or a superset of it with the same indices),
these elements could write their string table indices directly without hashing or
allocating.

### What "pre-seeding" would look like

**Option A: Copy input StringTable into output BlockBuilder**

1. At the start of `rewrite_block`, copy the input block's string table entries
   into the output BlockBuilder's StringTable
2. Maintain a mapping: input index → output index (identity if pre-seeded first)
3. For unmodified elements: translate input indices directly (no hash lookup)
4. For modified elements (from diff): use add() normally

Problem: the input block's `WireStringTable` stores `&[u8]` slices into the
decompressed buffer. Pre-seeding requires allocating `String` copies of every
entry — the same allocation we're trying to avoid. A typical block has ~1200
unique strings, so pre-seeding costs ~1200 String allocations upfront vs.
~21.8M/630 = ~34,600 add() calls per block. Pre-seeding is 35× fewer allocations
but still requires the FxHashMap insertions to maintain the index for later
add() calls from diff elements.

**Option B: Index passthrough mode**

A fundamentally different approach: add_node/add_way/add_relation accept raw
string table indices instead of `&str` for the base-element path.

```rust
// New method — accepts raw string table indices, no interning
fn add_node_raw_tags(&mut self, id: i64, lat: i32, lon: i32,
    raw_keys_vals: &[i32], raw_user_sid: i32) { ... }
```

This requires the merge code to:
1. Read raw varint-encoded tag indices from the input (not decode to `&str`)
2. Pass those indices directly to the output builder
3. Ensure the output string table matches the input's

This eliminates all hashing and allocation for base elements but requires:
- A dual-mode BlockBuilder (raw indices vs. string references)
- The merge code to maintain string table identity between input and output
- Handling the case where diff elements add new strings (extending the table)

### Estimated savings

With full index passthrough for base elements:
- Eliminate ~674ms of StringTable::add work (34% of rewrite_block)
- Eliminate ~167 MB of allocation churn
- New rewrite_block time: ~1.32s (down from 1.99s)
- **34% speedup on rewrite_block, ~19% speedup on merge wall time**

With simpler pre-seeding (Option A):
- Reduce per-element cost to an array index lookup (~5ns) instead of hash + alloc (~27ns)
- ~5× faster per element for string operations
- Estimated savings: ~540ms of the ~674ms StringTable cost
- **27% speedup on rewrite_block**

### Verdict

**Worth prototyping, but the implementation complexity is significant.** The
savings are real (~600ms on Denmark, proportionally larger at planet scale).
However:

1. Option A (pre-seed) still allocates — just fewer times and upfront
2. Option B (index passthrough) is the clean win but requires a new API surface
   on BlockBuilder and changes to merge's element processing
3. Both options only benefit `rewrite_block` in merge — the general write path
   (cat, sort, extract) cannot use index passthrough because elements come from
   decoded `&str` tags

**Profiling confirmed the model** (see "Measured cost per add() call" above):
27ns/call, 131M calls, 3.55s total, 80% of combined add_* time. Option A
(pre-seed) is the pragmatic first step (fewer changes, most of the win).

## TODO item: Raw packed bytes for refs/memids

### The theory

From TODO.md: "delta encoding is compatible (both input wire format and
BlockBuilder delta-encode refs/memids from 0 within each element), so raw byte
passthrough is valid."

Instead of decoding way refs (packed sint64 varints → Vec<i64>) and re-encoding
them (Vec<i64> → packed sint64 varints), pass the raw protobuf bytes through.

### Cost analysis

The non-StringTable cost per add_way is the remaining ~25%:

| Operation               | Estimated cost | × 2.4M ways | Total  |
|-------------------------|----------------|-------------|--------|
| Refs delta encode       | ~30ns          | 2.4M        | 72ms   |
| Proto Way construction  | ~40ns          | 2.4M        | 96ms   |
| **Subtotal**            | **~70ns**      |             | **168ms** |

For add_relation, member delta encoding:

| Operation               | Estimated cost | × 46K rels  | Total  |
|-------------------------|----------------|-------------|--------|
| Memids delta encode     | ~50ns          | 46K         | 2.3ms  |

### What raw passthrough would require

> **Update:** Direct wire-format encoding is now implemented. `add_way_raw` and
> `add_relation_raw` use manual protobuf field encoding via `src/write/wire.rs`.
> Raw packed bytes passthrough for refs/memids is now straightforward — just write
> the raw bytes directly to `packed_scratch` instead of delta-encoding from
> decoded `i64` values. See TODO.md for the unblocked item.

~~1. Bypass prost's generated `Way`/`Relation` types (which use `Vec<i64>`)~~
~~2. Write raw packed bytes directly into the protobuf output~~
~~3. Either: manual protobuf field encoding for refs/memids fields, or a custom
   prost message type with `Bytes` for these fields~~

The read side already has the raw bytes available (`WireWay.refs_data: &[u8]` in
`wire.rs`). ~~The difficulty is getting raw bytes into the write side, which uses
prost's generated types that expect `Vec<i64>`.~~ No longer blocked — write side
now uses direct wire encoding.

### Verdict

**Small but essentially free** now that direct encoding is in place. The savings
are ~72ms (3.6% of rewrite_block) for ways and ~2ms for relations. Combined
~74ms, or **3.7% of rewrite_block time**. Low priority but no longer complex.

Compared to StringTable optimization (~674ms, 34%), this is 9× less impactful.
The manual protobuf encoding adds fragile, hard-to-maintain code for a marginal
gain. **Skip this.**

## Summary: where rewrite_block's 1.99s goes

| Component                          | Estimated | % of 1.99s | Source             |
|------------------------------------|-----------|------------|--------------------|
| StringTable::add (hash + alloc)    | ~674ms    | 34%        | Profiled (80% of add_*) |
| take() serialization (within rw)   | ~400ms    | 20%        | Estimated          |
| Read-side decode + iteration       | ~300ms    | 15%        | Estimated          |
| Diff lookups (HashSet/HashMap)     | ~200ms    | 10%        | Estimated          |
| Refs/memids delta encoding         | ~170ms    | 9%         | Estimated          |
| Proto construction + Vec pushes    | ~130ms    | 7%         | Estimated          |
| CreateEmitter + type transitions   | ~130ms    | 7%         | Estimated          |

Note: take() timings here are the subset of the 676ms total that occur within
rewrite_block's flush_block calls (type transitions and capacity overflows), not
the full 676ms which includes passthrough boundary flushes and EOF flushes.

## Profiling results

`StringTable::add` instrumented with `#[hotpath::measure]` on cat --type path
(Denmark, full decode+write). Merge hotpath did not print — likely the pipelined
writer thread interferes with hotpath's atexit handler. Cat exercises the identical
BlockBuilder code path so results scale directly.

| Metric                 | Value           |
|------------------------|-----------------|
| Total add() calls      | 131,080,697     |
| Average time per call  | 27ns            |
| P50 / P95 / P99        | 20ns / 60ns / 170ns |
| Total time             | 3.55s           |
| % of wall time (cat)   | 5.76%           |
| % of add_* combined    | 80%             |

Key takeaway: the model's ~35ns estimate was slightly high — actual is 27ns. But
the 80% fraction of add_* time confirms StringTable::add is the dominant cost.
The P99 of 170ns shows occasional cache misses or allocator contention.

## Next steps

1. **Prototype Option A (pre-seed)** on a branch and benchmark merge with
   Denmark to see if the predicted ~27% rewrite_block speedup materializes
2. Occupied vs Vacant hit rate measurement is not needed — the profiled 27ns
   average is consistent with nearly all calls hitting the Occupied path
   (Vacant inserts with 2 allocations + HashMap insertion would average much
   higher)
