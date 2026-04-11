# Renumber Planet Scale

**Status (2026-04-11):** `pbfhogg renumber` is NOT planet-safe in its
current form — a naive single-pass implementation with three in-memory
`FxHashMap<i64, i64>` mappings that require ~278 GB of RAM at planet
scale. This document captures the analysis and sketches a planet-safe
external-join replacement modeled after `src/commands/external_join.rs`
(the ALTW external index). Parent: [altw-optimization-history.md](altw-optimization-history.md)
for the prior art that validated the same approach for the node-coord
join case.

## Problem

### Current implementation

`src/commands/renumber.rs` (153 lines, commit `cadc3e6`). Single-pass
sequential scan over a sorted PBF (nodes → ways → relations). For each
element:

1. Assign the next sequential new ID (`next_node_id++`,
   `next_way_id++`, `next_relation_id++`).
2. Insert `(old_id, new_id)` into one of three
   `rustc_hash::FxHashMap<i64, i64>` mappings (`node_map`, `way_map`,
   `relation_map`).
3. For ways: remap refs via `node_map.get(&old_node_id)`. For
   relations: remap members via the appropriate map per member type.

Source:

```rust
let mut node_map:     FxHashMap<i64, i64> = Default::default();  // renumber.rs:66
let mut way_map:      FxHashMap<i64, i64> = Default::default();  // renumber.rs:67
let mut relation_map: FxHashMap<i64, i64> = Default::default();  // renumber.rs:68
```

Simple, correct, planet-hostile.

### Memory math at planet scale

Planet snapshot 2026-02-23: 11.6B elements — 10.4B nodes, 1.17B ways,
14.1M relations.

hashbrown (rustc-hash's backend) uses a ~7/8 load factor with 1-byte
SSE2 control metadata per slot and 16-byte `(i64, i64)` entries. At
realistic load, including tombstones and the control-byte arena, the
effective cost is **~24 bytes per live entry**. Headline numbers:

| Map | Live entries | Bytes/entry | Total live | Total capacity (7/8 load) |
|---|---|---|---|---|
| `node_map` | 10.4B | ~24 B | ~250 GB | **~286 GB** |
| `way_map` | 1.17B | ~24 B | ~28 GB | ~32 GB |
| `relation_map` | 14.1M | ~24 B | ~340 MB | ~390 MB |
| **Total** | — | — | **~278 GB** | **~318 GB** |

On a 32 GB plantasjen host, running `brokkr renumber --dataset planet`
OOM-kills within the first ~60 seconds of the node scan. Extrapolating
from the hashmap growth curve: at ~1.2B nodes ingested (~12% of the
file), `node_map` has passed ~30 GB and the process dies before pass 2
(ways) is even reached. Not a useful regression datapoint — we already
know why.

It's also not safe on a "normal big server" either: a 256 GB host with
full swap would spend most of the run swap-thrashing `node_map`
insertions. `renumber` is structurally unbounded in RAM.

## Target

Planet `renumber` on a 32 GB host with bounded memory (<4 GB peak anon
RSS target, comparable to ALTW external stage 4 at ~1.6 GB). All I/O
sequential, no mmap thrash, no page faults. Temp disk footprint is
acceptable if it's in the same range as ALTW external (~300 GB at
planet scale — large but fits on a modern NVMe).

## Why the obvious fixes don't work

The same analysis arc as ALTW (`altw-optimization-history.md`) applies
directly, because renumber is structurally **another random-access
join** between two sorted streams: a "nodes sorted by old node ID"
stream and a "ways sorted by old way ID, but with refs spanning the
whole node ID space" stream. The refs within a single way blob are
scattered across the entire chronological node ID space, so:

- **Dense mmap index** (`old_node_id → new_node_id` as a file-backed
  mmap of `u64`, dimensioned by max node ID ≈ 13B). File size: 13B × 8
  bytes = 104 GB. Touched pages during pass 2: effectively all of it,
  because ways' refs span the full range. On a 32 GB host, same
  thrashing story as dense ALTW.
- **Sparse chunk-indexed array** (Planetiler-style). Same page-cache
  thrash on the values file as sparse ALTW at europe scale. Proven
  disproven in the ALTW investigation.
- **Partitioned multi-pass.** Split node ID range into N partitions,
  skip way blobs that don't reference the current partition. **Every
  way blob touches nearly every partition** (measured by
  `examples/partition_stats.rs` for ALTW: Japan N=64 median = 61-62 of
  64 partitions per way blob). Same failure mode here.
- **External join via double radix permutation.** Bounded memory, all
  sequential I/O, proven at planet scale for ALTW (1,462 s, 16.7 GB
  peak anon). This is the one that works.

The ALTW investigation did all this legwork already; renumber inherits
the conclusion for free.

## Recommended architecture

Three-pass external join, heavy reuse of `src/commands/external_join.rs`
primitives (`ScratchDir`, `BucketWriters`, `CooPair`, radix partition
by high bits of key).

### Key observation: `relation_map` stays in RAM

The planet relation count is 14.1M. A `FxHashMap<i64, i64>` for the
relation map is ~340 MB — fits comfortably in RAM alongside everything
else. Only `node_map` and `way_map` need external storage. This keeps
the pass-3 relation-member remapping simple.

### Pass 1 (nodes): stream + emit tuples + write renumbered nodes

Stream node blobs via `for_each_block_pipelined`. For each node:

1. Assign `new_node_id = start_node_id + node_rank` (monotonically
   increasing).
2. Write the renumbered node to the output PBF (via a `BlockBuilder` +
   `PbfWriter`, same as today).
3. Append `(old_node_id, new_node_id)` to a temp file `node_map.tuples`.

Because input is sorted by old node ID, the tuples file is also sorted
by old node ID — **no sort step required**. This is a crucial
simplification over ALTW stage 1 (which scatters way-ref tuples into
256 buckets to support the later merge-join).

Memory: negligible (streaming write, one `BlockBuilder` block buffer).
Temp disk: 10.4B × 16 bytes = **~166 GB**. Or, if we recognize that
`new_node_id` is just `start_node_id + file_offset / 8`, we can store
just `old_node_id` and the file is 83 GB. For merge-join simplicity in
pass 2, keeping the explicit `(old, new)` pair is probably worth the
2× disk cost.

### Pass 2 (ways): merge-join against node_map.tuples + emit way tuples + write renumbered ways

Stream way blobs via `for_each_block_pipelined`. For each way blob:

1. **Gather** all node refs from all ways in the blob into a single
   `Vec<(node_id, way_index_in_blob, ref_index_in_way)>`. One way blob
   has ~8000 ways × ~10 refs avg = ~80K refs.
2. **Sort** the refs by `node_id`. O(N log N) with N ≈ 80K — tiny.
3. **Merge-join** the sorted refs against the sorted `node_map.tuples`
   stream. Advance a file cursor through `node_map.tuples` while
   walking the sorted ref list. Emit `(way_index, ref_index, new_node_id)`
   results into a scatter buffer. O(N + M) where M is "node_map.tuples
   entries we walked past for this blob."
4. **Scatter** the results back to their original `(way_index,
   ref_index)` positions, giving each way in the blob a correctly-
   remapped ref list.
5. Assign `new_way_id` and write the renumbered way to the output PBF.
   Append `(old_way_id, new_way_id)` to `way_map.tuples`.

The merge-join cursor over `node_map.tuples` is **monotonically
advancing** across the whole pass 2 — we never seek backward. Within a
way blob, refs can go back to earlier node IDs, but since we buffer and
sort within the blob, each blob's merge-join starts fresh at the
cursor's current position. Between blobs, the cursor advances.

Wait — that's subtly wrong. A way in blob B might reference a node
whose tuple is at file offset 0 (old_node_id = 1), while the cursor
for the merge-join is at offset N (because blob A's ways already
walked past it). We can't rewind.

**Fix:** the merge-join cursor doesn't walk `node_map.tuples` linearly
across the whole pass 2. Instead, for each way blob, we re-open
`node_map.tuples` from the beginning and walk it once, matching
against the blob's sorted refs. That's 1 linear scan per way blob ×
~20K way blobs at planet scale × 166 GB file = **~3.3 PB of sequential
reads**. Obviously not acceptable.

**Real fix: use the 256-bucket radix pattern from ALTW.** Partition
`node_map.tuples` into 256 buckets by high bits of `old_node_id` in
pass 1 (exactly like ALTW stage 1 partitions COO pairs). Each bucket
is ~650 MB (166 GB / 256) and holds tuples for a contiguous slice of
the node ID space.

For pass 2, gather way refs into the 256 buckets by high bits of
`old_node_id`, sort each bucket, then for each bucket do a single
merge-join against the corresponding `node_map.buckets/N` file. The
per-bucket merge-join is **single-pass sequential** over a bounded
slice. This is the exact ALTW stage 2 pattern (`external_join.rs`
~line 400-600).

### Pass 2' (relations): merge-join against both maps

Two merge-join phases:

1. Stream relation blobs, gather **Node-member** refs into buckets by
   old node ID. For each bucket, merge-join against
   `node_map.buckets/N`, scatter results back.
2. Same relation blob scan, gather **Way-member** refs into buckets by
   old way ID. For each bucket, merge-join against
   `way_map.buckets/N`, scatter results back.
3. For **Relation-member** refs, consult the in-memory `relation_map`
   directly.

Assign `new_relation_id` and write the renumbered relation to the
output PBF. No need to emit relation tuples to disk — the in-memory
map is the end of the lookup chain.

### Self-referencing relations (forward refs)

**Gotcha:** a relation can reference another relation by ID, and the
referenced relation might have a **higher** ID (forward reference). In
a sorted PBF, the current relation is processed before the target
relation, so the `relation_map` entry for the target doesn't exist yet
when we try to remap the member.

Three options:

- **(a) Two passes over relations.** Pass R1 assigns new IDs and
  builds the in-memory map. Pass R2 re-reads relation blobs and does
  the actual member remapping with the complete map. Doubles relation
  I/O but relation volume is small (~14M relations, ~few hundred MB of
  blob data on planet) — this is the simplest and probably the right
  answer.
- **(b) Forward-reference queue.** Track relations whose remapping was
  incomplete, buffer them in memory until the missing target is
  assigned, then finalize. Memory-unbounded in the pathological case
  (long forward ref chains).
- **(c) Assume input ordering.** If we could guarantee relations only
  reference lower-ID relations, single-pass works. **Not guaranteed**
  in OSM data: super-route relations and nested admin boundaries
  routinely reference higher-ID targets.

Recommend (a). Add to the implementation plan as a deliberate second
relation pass.

### Output PBF assembly

Write the renumbered output in a streaming fashion via a single
`PbfWriter` across all three passes. Nodes stream out in pass 1, ways
in pass 2, relations in pass R2. The writer's internal block buffering
handles flushing, and no cross-pass retention is needed beyond the
writer itself.

## Temp disk estimate

| File | Planet size | Notes |
|---|---|---|
| `node_map.tuples` / 256 buckets | ~166 GB | 10.4B × 16 B pairs |
| `way_map.tuples` / 256 buckets | ~19 GB | 1.17B × 16 B pairs |
| Way-ref scratch (pass 2) | <1 GB peak | per-bucket sort buffer |
| Relation-ref scratch (pass R1) | <100 MB peak | far fewer refs |
| **Peak temp disk** | **~185 GB** | before cleanup |

After pass 2 completes, `node_map.tuples` can be deleted. After pass
R2, `way_map.tuples` can be deleted. Peak is reached during pass R1
when both map files are still present. Comparable to ALTW external's
~300 GB planet temp disk footprint.

## Wall time estimate

Rough extrapolation from ALTW external (1,462 s at planet scale) and
the renumber operation's shape:

| Pass | Estimate | Comparison |
|---|---|---|
| Pass 1 (nodes): scan + emit tuples + write | ~500 s | ALTW stage 1 was 333 s on planet; renumber adds output write |
| Pass 2 (ways): bucket merge-join + write | ~700 s | ALTW stage 2 + 4 combined was ~881 s (612 + 269); renumber does less work per ref (i64 lookup vs i32×2 coord lookup) but also writes PBF output |
| Pass R1 (relations, assign IDs) | ~30 s | tiny volume |
| Pass R2 (relations, remap + write) | ~50 s | merge-joins against bucket files |
| **Total** | **~1,300 s (~22 min)** | in the same ballpark as ALTW external and geocode-builder on planet |

This is a guess and will likely be off by ±50%. The real number depends
heavily on whether merge-join cursor walks dominate (sequential I/O)
or the output PBF write path dominates (compression CPU bound).

## Differences from ALTW external join

For the implementer: where this is easier and harder than ALTW
external.

**Easier:**

- Payload is `i64` (new node ID) not `(i32, i32)` coords. Same size
  (8 B) but simpler struct, no endianness subtlety, no `decimicro`
  conversion.
- `relation_map` stays in RAM (14M entries is trivial), so there's no
  third bucket-join phase for relation→relation lookups. ALTW has no
  equivalent of this — its join is purely node↔way.
- No "locations on ways" semantics to preserve; renumber doesn't touch
  coordinates.

**Harder:**

- **Three sequential output streams** (renumbered nodes, ways,
  relations) vs ALTW's one. The `PbfWriter` does handle this via
  streaming block flushes, but it needs careful pass-to-pass handoff
  and a header that's finalized before pass 1 begins.
- **Way IDs also need remapping** for the relation→way lookup chain.
  This means pass 2 has to emit `way_map.tuples` on top of doing the
  node lookups — a second emission target.
- **Two-pass relations** for forward-reference handling, vs ALTW's
  relation-passthrough (relations aren't touched by ALTW).
- **Output must be sorted and deterministic.** If different invocations
  of renumber on the same input produce different new ID assignments,
  downstream tooling breaks. The current implementation guarantees this
  via strictly sequential assignment in file order. The external join
  version must preserve the same ordering — easy as long as pass 1
  processes nodes in file order, but worth calling out in the test
  suite.

## Testing plan

1. **Unit: renumber correctness on Denmark** — existing
   `tests/renumber.rs` (320 lines) covers the current implementation.
   The external-join variant must produce **byte-identical output** on
   Denmark, or at minimum element-for-element identical output (same
   new IDs, same remapping). Add this as a cross-check.
2. **Unit: forward relation references** — construct a test PBF with
   a relation whose member references a higher-ID relation. Verify
   pass R2 handles the forward ref correctly.
3. **Cross-validation: `brokkr verify renumber`** — add a new verify
   subcommand that compares pbfhogg renumber output against osmium
   renumber. osmium's implementation handles planet (uses similar
   external techniques), so the comparison is a real sanity check.
4. **Bench: Denmark, Japan, Europe** — verify wall time doesn't
   regress from the in-memory path at sub-planet scales. The
   in-memory path should remain faster for small inputs (crossover
   expected between Japan and Europe, same as ALTW dense vs external).
5. **Bench: planet** — the actual target. Success criteria: completes
   without OOM, <4 GB peak anon RSS, wall time within 2× of the
   estimate above.

## Mode selection

Follow the `add-locations-to-ways --index-type dense|sparse|external`
pattern: add `--mode inmem|external` to renumber, defaulting to
`inmem` (the current implementation) for sub-Europe inputs and
`external` for Europe+. Autodetect the default based on input size,
with explicit override.

The `inmem` path stays as-is for correctness and simplicity on small
inputs where it's faster. The `external` path is the planet-safe
variant.

## Work breakdown

Rough estimate — comparable in scope to the ALTW external work
(several weeks, multiple commits):

1. **Skeleton + node pass** (~200 LoC, ~1 day) — new
   `src/commands/renumber_external.rs` reusing `ScratchDir`,
   `BucketWriters`, emit `(old_node_id, new_node_id)` pairs. Stream
   output nodes. Get a working Denmark reproduction of the node pass.
2. **Way pass merge-join** (~400 LoC, ~2-3 days) — port the ALTW
   stage 2 merge-join pattern, adapt for renumber's i64 payload,
   implement scatter-back, emit `(old_way_id, new_way_id)` pairs.
   Largest risk area — this is where ALTW spent most of its
   optimization effort.
3. **Relation two-pass** (~200 LoC, ~1 day) — pass R1 to build
   in-memory relation_map, pass R2 with bucket-joins against both
   node_map and way_map tuples.
4. **Integration + test** (~100 LoC + test harness, ~1 day) — wire
   into CLI, add `--mode` flag, unit tests, verify cross-check.
5. **Cross-validation vs osmium renumber** (~50 LoC + a new
   `brokkr verify renumber`, ~0.5 days).
6. **Planet bench + sidecar analysis** (~1-2 runs of ~20 min each, one
   session).
7. **Optimization** if planet bench exceeds the estimate by >2×:
   unknown, but ALTW external went through ~8 optimization passes
   from initial `302s → 12s` on Denmark. Budget ~1-2 weeks.

Total: **~1.5-3 weeks** depending on optimization depth. Much of this
is structural work that the ALTW external code has already de-risked.

## Open questions

1. **Is the `node_map.tuples` partition by high bits of `old_node_id`
   actually going to be well-balanced?** OSM node IDs are assigned
   chronologically, so the distribution is front-loaded (old IDs
   dense, new IDs sparse). ALTW hit this same question and the answer
   was "yes, balanced enough" but worth re-checking for renumber's
   slightly different access pattern.
2. **Can we share the `ScratchDir` / `BucketWriters` code with
   `external_join.rs` via extraction**, or should we duplicate the
   scaffolding into `renumber_external.rs`? Probably worth extracting
   into `src/external_radix.rs` or similar — both commands can share
   it and any future external-join commands would benefit. Not a
   blocker, can ship duplicated first and refactor later.
3. **How does osmium renumber handle planet?** Worth reading the
   libosmium source to see if they've chosen a different architecture
   (spatial index? b-tree on disk?). We should know what the reference
   implementation does before committing to our own.
4. **Sequential vs parallel merge-join in pass 2.** ALTW parallelized
   stage 4 (P2c, commit `6b09796`, 432 s → 136 s). Worth evaluating
   whether renumber pass 2 has similar parallelism headroom. The
   merge-join itself is I/O-bound on sequential reads, so probably
   yes — parallelize across buckets.
5. **Output PBF ordering guarantees.** Does renumber need to guarantee
   the output is sorted by **new** IDs (which it is under the current
   sequential assignment)? Yes — the sorted-flag in the header, once
   set, is a hard requirement. This constrains pass 1 to process nodes
   in **old** ID order (which it does naturally from sorted input).
6. **Renumber on unsorted input.** Current implementation rejects
   unsorted input via `require_sorted`. External-join variant should
   do the same. If unsorted input becomes a requirement later, the
   external-join path needs a sort pre-step (out of scope here).

## Code references

- `src/commands/renumber.rs` — current 153-line single-pass
  implementation. Baseline for correctness comparisons.
- `src/commands/external_join.rs` — 1,242 lines of reference
  infrastructure for bucket-based external joins. Key pieces:
  - `ScratchDir` (line 64) — managed scratch directory, auto-cleanup.
  - `BucketWriters` (line 98) — 256-way buffered bucket writers with
    flush-sync-fadvise-close semantics.
  - `CooPair` / `ResolvedEntry` (lines 162, 199) — binary pair
    encoding patterns to mirror.
  - Stage 1 way-pass (line ~300) — ref collection + bucket emission.
    Closest analogue to renumber pass 1's tuple emission.
  - Stage 2 merge-join (line ~400) — pread-from-workers sorted
    merge-join. Closest analogue to renumber pass 2's node lookup.
  - Stage 3 slot reorder (line ~700) — scatter buffer pattern. Not
    directly reused (renumber has no equivalent of slot reordering)
    but useful reference for the buffer management style.
- `tests/renumber.rs` — 320-line existing test suite. Any new
  external-join implementation must keep these passing.
- `cli/src/main.rs:717-1460` — CLI wiring (`run_renumber`,
  `RenumberOptions` parsing). Minimal changes needed to add a
  `--mode` flag.
- `notes/altw-optimization-history.md` — the prior-art investigation
  that validated external join for the analogous ALTW case. Read this
  before implementing; most of the lessons apply verbatim.
