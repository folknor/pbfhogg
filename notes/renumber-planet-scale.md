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

**Revised 2026-04-11 against the shipped implementation.** The
original table below was an underestimate — the full-review pass
enumerated every scratch file the pipeline actually creates and
totaled them against the real coexistence windows. Reviewers
reached consensus on a peak closer to 300-440 GB, not 185 GB.

### Original estimate (design-time, inaccurate)

| File | Planet size | Notes |
|---|---|---|
| `node_map.tuples` / 256 buckets | ~166 GB | 10.4B × 16 B pairs |
| `way_map.tuples` / 256 buckets | ~19 GB | 1.17B × 16 B pairs |
| Way-ref scratch (pass 2) | <1 GB peak | per-bucket sort buffer |
| Relation-ref scratch (pass R1) | <100 MB peak | far fewer refs |
| **Peak temp disk (design)** | **~185 GB** | optimistic |

### Measured design (what the shipped code produces)

The implementation writes every bucket set listed below. Coexistence
windows matter — at any given point, only a subset is live on disk
simultaneously. Stage-2b's per-bucket cleanup (cutting `way_ref`
buckets as their merge-join completes) is in place, so `way_ref`'s
full ~136 GB is never resident at peak. But `node_map + way_ref +
slot` do all coexist at the *start* of stage 2b before any per-bucket
cleanups land, because stage 2a finishes writing all 256 `way_ref`
buckets before stage 2b begins.

| File | Planet size | Lifetime |
|---|---|---|
| `node_map-NNN` × 256 | ~166 GB | Pass 1 → end of renumber_external |
| `way_ref-NNN` × 256 | ~136 GB | Stage 2a → per-bucket cleanup during stage 2b |
| `slot-NNN` × 256 | ~136 GB | Stage 2b → stage 2c consumption |
| `new_refs` flat file | ~83 GB | Stage 2c → end of stage 2d |
| `way_map-NNN` × 256 | ~19 GB | Stage 2d → end of renumber_external |
| `way-ref-counts` sidecar | ~200 KB | Stage 2a → stage 2d |
| `rel-node-ref` + `rel-way-ref` buckets | ~3 GB | R1+R2a fused → R2b |
| `rel-node-slot` + `rel-way-slot` buckets | ~3 GB | R2b → R2c |
| `rel-node-new-refs` + `rel-way-new-refs` flat files | ~1.6 GB | R2c → R2d |

**Peak coexistence windows**:

1. **Start of stage 2b**: `node_map` (166 GB) + `way_ref` (136 GB) +
   empty `slot` buckets = ~302 GB.
2. **End of stage 2b**: per-bucket `way_ref` cleanup has run, but
   the last bucket's way_ref may still be in flight (~0.65 GB). Plus
   full `node_map` (166 GB) + growing `slot` (up to 136 GB). Peak:
   ~302 GB again, approximately.
3. **Stage 2c**: `node_map` (166 GB) + `slot` (136 GB) + growing
   `new_refs` (up to 83 GB). Peak before slot_buckets cleanup:
   ~385 GB.
4. **Stage 2d start**: `node_map` (166 GB) + `new_refs` (83 GB) +
   empty `way_map` = ~249 GB.
5. **Stage 2d end**: `node_map` (166 GB) + `new_refs` (83 GB) +
   full `way_map` (19 GB) = ~268 GB.
6. **R1+R2a fused**: previous state + `rel-node-ref` + `rel-way-ref`
   = ~271 GB.
7. **R2b/R2c/R2d**: similar, ~275 GB peak.

**Peak temp disk: ~385 GB** during stage 2c before `slot_buckets`
cleanup. Close to the ALTW external's ~300 GB planet footprint,
slightly higher because renumber has a richer set of intermediate
files (`new_refs` flat + 4 relation-member bucket sets) that ALTW
doesn't.

**Budget implication**: the plantasjen host's data drive needs at
least 450-500 GB free before running planet renumber. On a host
with 180 GB NVMe (which might be tempting to optimize for), this
won't fit. Document in the README planet-scale table once the
actual bench lands.

### Mitigations

- **Shipped**: per-bucket `way_ref` cleanup during stage 2b loop
  (cuts peak by ~136 GB vs batch cleanup).
- **Followup (tracked in TODO.md)**: sparse-file `new_refs` via
  `set_len` + `pwrite` so the flat file backs only populated slots.
  Would remove ~83 GB from the peak at stage 2c.
- **Followup**: delete `slot_buckets` per-bucket during stage 2c
  scatter, analogous to the stage 2b cleanup. Cuts another ~136 GB
  from the peak 3 window.

With both followups applied, peak drops toward ~250 GB, roughly
matching the ALTW external budget.

## Wall time — design estimate vs first measurement

**Design estimate** (pre-implementation, extrapolated from ALTW
external's 1,462 s planet run):

| Pass | Estimate |
|---|---|
| Pass 1 (nodes) | ~500 s |
| Pass 2 (ways) | ~700 s |
| Pass R1 (relations, assign IDs) | ~30 s |
| Pass R2 (relations, remap + write) | ~50 s |
| **Total estimate** | **~1,300 s (~22 min)** |

### First planet measurement: 2026-04-11, commit `e156e97`, UUID `c5d00c22`

**`brokkr renumber --dataset planet --mode external --bench 1` on
plantasjen** (AMD Ryzen 9 5900X, 30 GB DDR4, 24 GB avail, NVMe data
drive, performance governor, kernel 7.0.0-12).

| Phase | Duration | Peak Anon RSS | Share of total |
|---|---:|---:|---:|
| `RENUMBER_EXT_PASS1` (nodes) | **1,147 s (19.1 min)** | 130 MB | **33.2%** |
| `RENUMBER_EXT_STAGE2A` (way COO emit) | 339 s (5.6 min) | 131 MB | 9.8% |
| `RENUMBER_EXT_STAGE2B` (node merge-join) | **823 s (13.7 min)** | **2.79 GB** | **23.8%** |
| `RENUMBER_EXT_STAGE2C` (slot reorder) | 174 s (2.9 min) | 1.27 GB | 5.0% |
| `RENUMBER_EXT_STAGE2D` (way assembly) | **664 s (11.1 min)** | 1.19 GB | **19.2%** |
| `RENUMBER_EXT_R1_R2A` (rel assign+emit) | 31 s | 487 MB | 0.9% |
| `RENUMBER_EXT_R2B` (rel merge-join) | 236 s (3.9 min) | 1.98 GB | 6.8% |
| `RENUMBER_EXT_R2C` (rel slot reorder) | 2 s | 419 MB | <0.1% |
| `RENUMBER_EXT_R2D` (rel write) | 33 s | 431 MB | 1.0% |
| **TOTAL** | **3,456 s (57.6 min)** | **2.79 GB peak anon** | — |

Memory trajectory (100 ms sidecar samples, n=34,566): min 0, **max
2,785,084 kB (2.79 GB)**, avg 721 MB, p50 122 MB, p95 2.45 GB. The
2.79 GB peak lives entirely in stage 2b; every other stage runs with
<200 MB anon.

Element counters (all verified against expected planet scale):

- nodes: 10,447,738,627
- way refs (total slots): 12,435,459,911
- ways: 1,165,589,744
- relations: 14,124,889
- relation node members: 22,732,221
- relation way members: 136,900,241

### Analysis: 2.6× over design estimate

The measured 57.6 min is 2.6× over the 1,300 s design estimate. The
design doc caveat "will likely be off by ±50%" understated it. Where
the time went:

**Pass 1 (1,147 s, design est. 500 s): 2.3× over.** The node scan is
single-threaded end-to-end — `BlobReader` reads blob-by-blob, single-
thread decompress, single-thread parse, `BlockBuilder::add_node` in a
tight loop, output through the pipelined writer. At planet's 10.4B
nodes, each node crosses ~6 function boundaries in the hot path.
Effective I/O throughput during pass 1 was ~50 MB/s on an NVMe that
can do 3+ GB/s, so the bottleneck is CPU per node (~110 ns/node)
rather than disk. The comparable `pbfhogg cat` planet run takes ~500 s
because it uses the pipelined reader with parallel decompression;
pass 1 doesn't. **Pass 1 parallelization is the biggest available win.**

**Stage 2b (823 s, design est. ~400 s of the ~700 s "pass 2"): as
predicted by the review.** The full-review pass flagged
`sort_unstable_by_key` on 40M-entry buckets × 256 buckets as the
"likely planet wall-time floor" and pointed at radix sort. Measured
823 s is exactly in the predicted range. Radix sort over the 5-byte
key range would run in linear time; expected speedup ~8×, bringing
stage 2b toward ~100 s. **Second biggest available win.**

**Stage 2d (664 s, design est. ~300 s of pass 2): 2.2× over.** Same
single-threaded pattern as pass 1 — pread a way blob, decompress,
walk each way's refs via mmap'd `new_refs`, push to `BlockBuilder`,
write. Parallel decode would help here too.

**Everything else** (stages 2a, 2c, all relation phases) ran under
the collective budget of the design estimate and isn't worth
optimizing.

**Peak memory 2.79 GB vs 4 GB target: 30% under budget.** No
memory-bound concern. Temp disk peak came in well under the 912 GB
available and is not worth re-measuring precisely from this run
(sidecar doesn't track disk).

### Optimization roadmap — reviewer consensus (2026-04-11)

After the first planet measurement, I sent a brief to the planet +
perf + arch reviewers (claude + codex per archetype) asking for
optimization guidance to hit ≤20 min wall. The review landed two
**new** levers I hadn't considered and revised my own estimates
down:

**Reviewer findings:**

1. **My stage 2b `823 → 100 s` estimate was too optimistic.** The
   sort isn't the whole cost. Stage 2b also pays ~170 s of sequential
   bucket I/O (650 MB × 2 sides × 256 buckets at ~3 GB/s NVMe floor)
   and ~100 s of parse overhead. Radix sort eliminates the ~500 s
   sort CPU cost but the I/O floor remains. **Realistic target with
   radix sort alone: 823 → ~350 s.**

2. **My pass 1 `1147 → 400 s` estimate was too optimistic.** `cat`
   at ~500 s on planet is raw-frame passthrough, not a fair
   comparator. Pass 1 does full decode + re-encode + 166 GB scratch
   emission. **Realistic target with parallel decode: 1147 → ~600 s.**

3. **New lever A: halve the map-bucket record format.** Currently
   pass 1 emits `IdPair { old_id, new_id }` at 16 bytes per node;
   stage 2d emits the same shape per way. But `new_id` is derivable
   from `start_id + cumulative_bucket_index`, because input is sorted
   and bucket ranges are monotonic. Store just `old_id` (8 bytes).
   The design doc flagged this option at line 132 and deferred it
   "for merge-join simplicity." Given measured planet numbers, the
   trade is worth taking back. Halves:
   - Pass 1 scratch writes (166 GB → 83 GB) — save ~200 s
   - Stage 2b read I/O — save ~80 s
   - Stage 2d way_map writes — save ~50 s
   - R2B read I/O — save ~30 s
   - **Stage 2b per-bucket RAM (650 MB → 325 MB)**, enabling lever B

   **Total direct savings: ~360 s.** Flagged by claude-arch and
   codex-perf independently.

4. **New lever B: bucket-level parallelism in stage 2b.** The 256
   buckets are embarrassingly parallel. 2 worker threads × radix sort
   = ~180 s vs ~350 s sort-alone. Memory: 2 workers × 325 MB per side
   × 2 sides = 1.3 GB peak (within the 4 GB target). 4 workers would
   need the map shrink to fit under budget; 2 is safer. Flagged by
   codex-perf.

**Unanimous sequencing (all three reviewers):**

1. **Stage 2b radix sort first.** Self-contained, one function in
   one file, element-equivalence tests verify immediately. Clean
   measurement loop before structural changes.
2. **Map record format shrink.** Cross-cutting (touches 4-5 call
   sites) but unlocks memory headroom for bucket-level parallelism
   and halves I/O in pass 1, stage 2b, stage 2d, R2B simultaneously.
3. **Bucket-level parallelism in stage 2b.** Builds on #1 + #2.
4. **Build one shared schedule + pread + worker decode + reorder
   ordered-write helper.** Apply to pass 1, stage 2d, stage 2a in
   that order.
5. **R2B radix sort** (mirror of 2b, smaller N).

**Parallel decode pattern choice**: **not** `for_each_block_pipelined`.
All three reviewers unanimous: use the schedule + pread + worker
decode pattern from `external_join.rs` / `extract.rs` /
`tags_filter.rs`. The pipelined reader has cross-thread
`PrimitiveBlock` retention issues documented in
`notes/parallel-classify-regression.md` that the pread pattern
avoids. Worker-local alloc/free stays bounded, ordered emission is
explicit via `reorder_buffer::ReorderBuffer`.

### Revised theoretical roadmap

| Phase | Baseline | Target | Optimization | Status |
|---|---:|---:|---|---|
| PASS1 nodes | 1,147 s | ~500 s | parallel decode + map shrink | ✅ landed (commits `a478ae8`, `8ec298c`) |
| STAGE2A way emit | 339 s | ~150 s | parallel scan (worker pool) | ✅ landed (commit `e7219f0`) |
| STAGE2B node merge-join | 823 s | ~150 s | radix sort + 2-worker parallelism + map shrink | ✅ landed (commits `cc80442`, `a478ae8`, `37ff902`) |
| STAGE2C slot reorder | 174 s | 174 s | unchanged | — |
| STAGE2D way assembly | 664 s | ~300 s | parallel decode + map shrink | ✅ landed (commits `a478ae8`, `34a6b7c`) |
| R1+R2A fused | 31 s | 31 s | unchanged | — |
| R2B rel merge-join | 236 s | ~90 s | radix sort + map shrink | ✅ landed (commits `cc80442`, `a478ae8`) |
| R2C + R2D | 35 s | 35 s | unchanged | — |
| **TOTAL** | **3,456 s (57.6 min)** | **~1,430 s (23.8 min)** | **~2,026 s saved** | awaiting measurement |

**Honest target: ~24 min after all seven wins land.** All landed as
of commit `e7219f0` (2026-04-11). Planet bench in flight on this
commit for ground-truth measurement — results to be added to the
"Planet bench series" section below once they land.

The remaining gap to 20 min would require an output compression
change (zlib:6 → zstd:1 or `--compression none`, save ~400 s per
claude-perf), but **the production pipeline already uses
`--compression none`**, so the published 24 min figure is for the
zlib:6 default path and production-relevant numbers will be
correspondingly faster. 24 min is the accepted target.

**Rollout cadence:** each optimization was committed independently
and smoke-tested on Denmark (`brokkr verify renumber --dataset
denmark`, 306-relation-member orphan delta preserved exactly across
every commit). Planet was deferred to a single bench run on the
final commit to amortize the ~1 hour measurement cost.

### Second planet measurement: 2026-04-12, commit `f607842`, UUID `d8330e2a`

**All seven optimizations landed.** `brokkr renumber --dataset planet
--mode external --bench 1` on plantasjen (same host as first
measurement).

| Phase | Baseline | Measured | Δ |
|---|---:|---:|---|
| PASS1 nodes | 1,147 s | **676 s** | −471 s (−41%) |
| STAGE2A way emit | 339 s | **133 s** | −206 s (−61%) |
| STAGE2B node merge-join | 823 s | **427 s** | −396 s (−48%) |
| STAGE2C slot reorder | 174 s | 197 s | +23 s |
| STAGE2D way assembly | 664 s | **391 s** | −273 s (−41%) |
| R1+R2A fused | 31 s | 30 s | ≈ |
| R2B rel merge-join | 236 s | **138 s** | −98 s (−42%) |
| R2C + R2D | 35 s | 35 s | ≈ |
| **TOTAL** | **3,456 s (57.6 min)** | **2,033 s (33.9 min)** | **−1,423 s (−41%)** |

Peak anon RSS: **7.31 GB** (up from 2.79 GB). The growth is in stage
2b — work-stealing dispatch means `load_old_id_bucket_shards`
concatenates two interleaved shards and radix-sorts the combined
vector. Two workers × (way_refs + scratch + node_map + node_map_scratch)
≈ 3.4 GB per worker ≈ 6.8 GB peak. Well under the 30 GB host
limit.

Element counters all match the baseline exactly: 10,447,738,627
nodes / 12,435,459,911 way refs / 1,165,589,744 ways / 14,124,889
relations.

### Analysis: 33.9 min vs 23.8 min target

Missed the stretch target by ~10 min. Stage 2b is the main gap
(427 s measured vs 150 s projected). Three factors:

1. **Node_map radix sort.** The range-based dispatch used in commits
   `8ec298c`–`e7219f0` OOMed at 26 GB anon RSS because the single
   `ReorderBuffer` accumulated worker B's entire backlog while worker
   A's range drained (118 MB/s linear growth, killed at t=295 s). The
   fix (commit `f607842`) switched to work-stealing dispatch, which
   keeps the reorder gap bounded at O(64) — but shards now interleave
   in id space, requiring a `radix_sort_ids` pass after concatenation.
   The original roadmap assumed sorted-concat from disjoint ranges
   which would have been free.
2. **Double shard I/O.** With work-stealing, each shard contains an
   arbitrary subset of the input's node blobs. Stage 2b reads both
   shards' files for every bucket — 2× the disk I/O per bucket vs
   the old single-node_map path.
3. **Higher per-bucket RAM.** Combined shards + radix scratch ≈ 3.4
   GB per worker. Planet's 7.31 GB peak is exactly here.

Remaining levers if the 24-min target is revisited:
- **Sorted merge instead of concat + sort.** Each shard is internally
  sorted (work-stealing pulls in FIFO order, PBF is sorted). A 2-way
  merge would eliminate the `radix_sort_ids` pass entirely. Estimated
  savings: ~50–100 s on stage 2b.
- **More workers (4 instead of 2)** for pass 1 / stage 2d. Smaller
  per-shard data but more shard files for stage 2b to read. Trade-off
  needs measurement.
- **`--compression none`** for the production path. Eliminates writer
  backpressure entirely (the permit-pool from `9695ad5` is still in
  place as a safety net). The published 33.9 min is with zlib:6;
  `--compression none` should be measurably faster.

### Third measurement: 1,901 s (31.7 min), commit `d3da65f`, UUID `7372cddb`

Two-cursor merge (replaces concat + radix sort in stage 2b) and
`PrimitiveBlock::from_vec_with_scratch` (eliminates `.to_vec()` copy
in pass 1 and stage 2d workers). Stage 2b: 427→366 s. R2B: 138→70 s.
Peak anon: 7.31→6.57 GB. Total: 2,033→1,901 s (−6.5%).

### Pass 1 deep-dive (2026-04-12, dirty iteration with early exit)

Instrumented pass 1 with per-phase counters (pread / decompress /
parse / process / send on workers, write timing on consumer). Five
planet runs across different configurations. Key data:

**Best run: 4 workers, `add_node`, `MALLOC_ARENA_MAX=2` → 416 s.**

| Metric | Cumulative | Per-worker |
|---|---:|---:|
| pread | 123 s | 31 s |
| decompress | 226 s | 57 s |
| parse | 9 s | 2 s |
| **process** | **1,174 s** | **294 s** |
| send | 0 s | 0 s |
| consumer write | 16 s | (single thread) |

Process = `pass1_process_blob` = `block.elements()` iteration +
`bb.add_node` (delta encode + string table HashMap + metadata) +
`ensure_node_capacity_local` + shard bucket `write_all(8 bytes)`.
At 10.4B nodes = **113 ns/node**.

**Glibc arena fragmentation confirmed.** Without `MALLOC_ARENA_MAX=2`,
anon RSS grows linearly at 118 MB/s to 26 GB (OOM threshold). Cause:
OwnedBlock `Vec<u8>` allocated on pass1 worker thread, freed on rayon
compression thread. `MALLOC_ARENA_MAX=2` caps this at 486 MB with
negligible contention penalty at 4 workers.

**`add_node_raw` + `pre_seed_string_table` is a regression.** Process
went from 1,174 → 3,617 s with ARENA=2. The per-block `pre_seed`
cost (~150 `Rc::from` allocs + HashMap inserts × 1.3M blocks) exceeds
the per-node tag-lookup savings. Abandoned.

**Cache analysis (planet-claude).** `add_node` touches ~12 memory
regions per node. Working set per 8000-node block ≈ 536 KB — barely
fits Zen 3 L2 (512 KB). Every access is an L1 miss at ~5 ns. Estimated
L1-miss cost: 780 s cumulative, likely the dominant process expense.

### DenseNodes wire-format rewriter (next step)

Unanimous perf + planet reviewer recommendation: stop using
`BlockBuilder` for pass 1. Build a renumber-specific `reframe_dense_
with_new_ids` function that operates at the wire-format level:

1. Decompress blob → raw protobuf bytes.
2. Parse DenseNodes message just enough to locate the packed ID field
   (field 1, wire type LEN) and other payload fields.
3. Decode old IDs (zigzag varints) to count nodes and emit old_ids
   into the node_map bucket shard.
4. Generate new packed ID deltas: sequential renumber means delta = 1
   for all but the first node. `zigzag(1) = 0x02`, repeated N−1
   times. First delta = `zigzag(new_block_start_id)`.
5. Copy lat/lon/keys_vals/denseinfo raw bytes verbatim.
6. Copy the input block's StringTable bytes verbatim.
7. Re-frame as PrimitiveBlock: field 1 = copied StringTable, field 2 =
   PrimitiveGroup containing the reconstructed DenseNodes.
8. The output block has different byte length from the input (because
   the new ID deltas have different varint widths), so enclosing
   message lengths must be recomputed. But the lat/lon/tag/info
   payload bytes are bit-identical to the input.

**What this eliminates:** all 12 dense arrays in BlockBuilder, the
string table HashMap, metadata construction, tag iteration,
`add_node`. Per-node cost drops from ~113 ns to ~10-15 ns.

**Estimated savings:** process 1,174 → ~200 s cumulative. At 4
workers: ~50 s wall for process (down from ~294 s).

**Risk:** blocks with non-default granularity (field 17 ≠ 100),
lat_offset (field 19 ≠ 0), or lon_offset (field 20 ≠ 0) must be
detected and handled by falling back to the full decode+re-encode
path. All standard planet PBFs use defaults. Add an assertion.

### Fourth measurement: 1,468 s (24.5 min), commit `dc13a7b`, UUID `4d0e2c17`

DenseNodes wire-format rewriter + 4 pass-1 workers + mallopt landed.

| Phase | Baseline | Previous | **Final** | Δ vs baseline |
|---|---:|---:|---:|---:|
| PASS1 nodes | 1,147 s | 666 s | **168 s** | **−85%** |
| STAGE2A way emit | 339 s | 132 s | **129 s** | −62% |
| STAGE2B merge-join | 823 s | 366 s | **382 s** | −54% |
| STAGE2C slot reorder | 174 s | 202 s | 224 s | +29% |
| STAGE2D way assembly | 664 s | 394 s | **418 s** | −37% |
| R1+R2A fused | 31 s | 30 s | 29 s | — |
| R2B rel merge-join | 236 s | 70 s | **68 s** | −71% |
| R2C + R2D | 35 s | 35 s | 40 s | — |
| **TOTAL** | **3,456 s** | 1,901 s | **1,468 s** | **−57%** |

Peak anon: **7.04 GB**. All element counts match baseline exactly.

Pass 1 is no longer the bottleneck — it's now the fastest stage at
168 s (11% of total). The new bottleneck is **stage 2d (418 s, 28%)**
followed by **stage 2b (382 s, 26%)**. Stage 2d is the next candidate
for the same wire-format rewriter treatment (way IDs change but refs
are already resolved via the new_refs flat file — a specialized
rewriter could patch only the ID + refs fields).

### Fifth measurement: 401 s (6m42s), commit `71bb548`

Session 2 (2026-04-12). Three commits on top of the IdSetDense
rank-fusion architecture (442 s baseline from commit `94bf351`):

1. **Dead code cleanup** (`7df705c`): deleted 1366 lines of orphaned
   CooPair/bucket infrastructure. No behavioral change.
2. **Wire-format splice rewriter for R2d** (`cbffb45`): patches relation
   id (field 1) + member ids (field 9, delta-encoded), copies all other
   fields verbatim. Walks types + memids in parallel for member dispatch.
   R2d: 31.5 → 24.7 s.
3. **Parallel R2d** (`71bb548`): work-stealing dispatch with per-blob
   member-count sidecar from the fused relation scan. R2d: 24.7 → 18.4 s.

| Phase | Previous (442 s) | **Current (401 s)** | Δ |
|---|---:|---:|---|
| PASS1 (4 workers) | 145 s | **147 s** | noise |
| FUSED_WAY | 89 s | **88 s** | — |
| STAGE2D (6 workers) | 126 s | **101 s** | −25 s |
| R1+R2A | 28 s | **29 s** | — |
| R2D | 31.5 s | **18 s** | **−13 s (−42%)** |
| **TOTAL** | **442 s** | **401 s** | **−41 s (−9%)** |

Peak anon: 9.62 GB. All element counts match baseline exactly. Denmark
cross-validated (PASS, identical to osmium sort) on every commit.

**Attempted and reverted:** parallel fused-way direct-write (two-phase:
parallel ref-count pre-scan + parallel pwrite). Regressed 88 → 117 s
because decompressing way blobs twice costs more than the parallel
writes save. Workers are the bottleneck, not the sequential consumer.

**Next opportunity:** fuse the fused-way scan + stage 2d into a single
pass. Currently decompresses every way blob twice (once for ref
resolution, once for wire-format splice). Merging them with inline
rank() resolution during the splice would eliminate one full decompress
pass (~80 s estimated savings).

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

## Correctness review findings (2026-04-11)

Pre-implementation correctness review (`review correctness`, claude + codex)
on the design and the current `src/commands/renumber.rs`. Six questions,
six concrete answers. Actionable findings:

### 1. Target element-identical output, not byte-identical.

Byte-identical is unachievable in general — BlockBuilder rebuilds the
string table in encounter order, DenseNodes re-packs with its own delta
encoding, and block flush boundaries depend on the `ensure_*_capacity`
call timing (which legitimately differs between in-memory and external
paths). Even two runs of the current in-memory path wouldn't be
byte-identical if the string table has duplicates in different order.

**Cross-check is element-equivalence**: read both outputs via
`ElementReader::from_path`, collect `(type, id, tags as BTreeMap, refs
as Vec, members as Vec, metadata)` tuples sorted by new id, `assert_eq!`.
Extend `tests/common/mod.rs` helpers. Do **not** compare on-disk bytes.

### 2. The scatter-back pattern must key on `(elem_index, subindex)`.

Sorted-output invariant holds — file-order emission + monotonic ID
assignment preserves sortedness. The trap is **within** a way or
relation: refs/members must be rewritten at their original positions.

Specifically, if a way has refs `[A, B, A]` (same node twice, in
positions 0 and 2), sorting by `old_node_id` alone collapses them, and
scatter-back without `ref_index` as secondary key produces wrong
ordering. The COO pair for the way pass is `(old_node_id, way_index,
ref_index)`, not just `(old_node_id, slot_pos)` — explicit ref_index
preservation is mandatory, not optional.

Add a unit test on a way with `[A, B, A]` ref pattern.

### 3. ⚠️ Pre-existing bug in in-memory `renumber.rs:135` (forward relation refs). ✅ Fixed 2026-04-11.

```rust
MemberId::Relation(id) => MemberId::Relation(
    relation_map.get(&id).copied().unwrap_or(id)
),
```

When a relation at old_id=500 references old_id=600 (forward ref, not
yet assigned), the lookup returns None, `unwrap_or(id)` falls through to
the OLD id 600, and **the old id is written into the new output** where
it either collides with a different renumbered relation or dangles as
an orphan reference.

Existing test `renumber_relation_referencing_relation` at
`tests/renumber.rs:274` only covers the **backward-ref** case (rel 600
references rel 500, backward), not the forward case. Bug is
pre-existing and latent.

**Both paths must be two-pass.** The external path already solves it
structurally via R1→R2 (relation_map is fully built before R2 reads
it). The in-memory path now does the same: Pass 1 scans the input
streaming nodes+ways to output and assigning relation IDs without
writing; Pass 2 reopens the input, fast-skips non-relation blobs via
the blob index, and writes relations with the now-complete map.

**Resolution.** Shipped in-memory two-pass refactor 2026-04-11.
`src/commands/renumber.rs` now has Pass 1 (nodes + ways + assign
relation IDs) and Pass 2 (reopen input, fast-skip via `blob.index()`,
remap relation members, write). Regression tests added to
`tests/renumber.rs`:

- `renumber_relation_forward_ref` — rel 500 → member Relation(600),
  rel 600 empty. Output asserts new rel 500 member references new rel
  600's new id (not old 600). **This test fails against the pre-fix
  single-pass code** (reproduced locally: `left: 600, right: 2`).
- `renumber_relation_self_reference` — rel 42 → member Relation(42).
  Output asserts new id 1 references itself.

Denmark wall time: 19.3s on the refactored path, within the historical
18.8-19.3s noise band (baseline commits `b685342`, `f9ba88d`, `8e8240e`,
`b45b731`, `6b74436`). The second scan is effectively free at this
scale because pass 2 fast-skips ~300 node/way blobs via `blob.index()`
and only decompresses the ~5-10 relation blobs at the end of the file.

### 4. Relation member order preservation: `Vec<Option<Member>>` per relation, indexed by position.

The three merge-join phases (node members, way members, relation members)
each write into the **same** pre-allocated slot array per relation, at
stable `member_index` positions. Never append in join order. Never
filter. Never rebuild. Final emission reads sequentially by index.

Concrete pattern:
```rust
let mut members_out: Vec<Option<RemappedMember>> =
    (0..original.members().count()).map(|_| None).collect();
// Phase 1: node members
for (rel_idx, mem_idx, new_id) in ... { members_out[mem_idx] = Some(...); }
// Phase 2: way members (same slot array)
// Phase 3: in-memory relation members
// Emission: members_out.into_iter().map(Option::unwrap).collect()
```

### 5b. Orphan-reference handling diverges from osmium. ⚠️

**Measured 2026-04-11 via `pbfhogg diff` against osmium renumber output
on Denmark.** pbfhogg and osmium agree on 59,151,976 of 59,152,282
elements (100.0% of nodes and ways, 99.3% of relations). 306 relations
differ, and **the only differing field on each is the member list**.
No tags, coords, refs, or metadata differ.

The 306 differences are all orphan-reference handling — relation
members pointing to objects not present in the input (e.g., Denmark
relations with members referencing ways in Germany/Sweden/Norway).
The two tools resolve orphans differently:

- **pbfhogg**: `resolved_id = old_id`. The output contains a mix of
  new-space ids (for in-input targets) and old-space ids (for
  orphans). Downstream tools that assume contiguous new ids must
  tolerate the mixed space.
- **osmium**: assigns **new** sequential ids to orphan targets via
  its `id_map::m_extra_ids` overflow table (bespoke `id_map` class
  in `osmium-tool/src/command_renumber.cpp`, per the prior-art
  research earlier in this doc). The ids continue past the last
  in-input id for each type, so a Denmark run with 6,616,526 ways
  emits orphan way refs as 6,616,527, 6,616,528, ... in the order
  they're first encountered. Ensures contiguous new-space output
  at the cost of assigning ids to objects that don't exist in the
  output.

**This reverses the claim earlier in this doc that "orphan refs
match in-memory behavior and osmium's behavior."** The in-memory/
external agreement holds (both preserve old ids for orphans), but
the "matching osmium" half was wrong — I didn't verify that empirically
until now.

**Which is correct?** Both are defensible. osmium's choice is
cleaner for downstream contiguous-id assumptions but introduces
"phantom" ids referring to nothing. pbfhogg's choice preserves the
original id space boundary but produces mixed output.

**Decision**: ship pbfhogg's current behavior and document it as a
known cross-validation delta. Matches the existing pattern for
other semantic differences in the README (extract relation inclusion,
diff 14-element comparison, check-refs occurrences vs unique).
Users who need osmium-compatible orphan handling can add a followup
`--orphan-policy assign|preserve` flag.

The 306 Denmark relations affected are all transboundary admin
boundaries, route relations, and TMC (Traffic Message Channel)
segments — all expected to have cross-border member references.
Planet will have many more orphan refs (every country extract has
cross-border relations), but the structural behavior is unchanged.

**Verification command** (used manually, pending `brokkr verify
renumber` from the brokkr dev):

```
osmium renumber data/denmark-*.osm.pbf \
    -o data/bench-tmp/osmium-out.osm.pbf --overwrite
brokkr renumber --dataset denmark --mode external --force
# manually move brokkr's output aside before the next run
pbfhogg diff data/bench-tmp/osmium-out.osm.pbf \
    data/bench-tmp/bench-renumber-external-output.osm.pbf -s -c
# → Summary: left=59152282 right=59152282 same=59151976 different=306
```

### 5. Reject negative IDs in external mode with a clear error.

Design decision: external path requires non-negative input IDs. Detect
at stage 1 entry, error out with:

```
error: `renumber --mode external` requires non-negative input IDs.
       Input contains node id -42 at offset 0x...
       Use `--mode inmem` for files with negative (editor-local) IDs.
```

Rationale:
- Production planet/region data never contains negative IDs — they are
  editor-local (JOSM staging) identifiers resolved before upload.
- The in-memory path handles negatives transparently via FxHashMap and
  is retained as the sub-Europe default, so users with negative-ID
  input lose no functionality.
- Partition-by-high-bits clamps negatives to bucket 0, which is a
  bucket imbalance, not a semantic error — as long as intra-bucket
  sort/merge-join use signed i64, it would "work." But rejecting is
  simpler than guaranteeing correctness across the negative/positive
  boundary in a future change, and gives users a clear error path.

Test: input with negative node IDs → external mode errors, in-memory
mode succeeds.

### 6. Revised test matrix.

Minimal set for the external-path test harness (additive to the existing
320-line suite in `tests/renumber.rs`):

- **Forward-ref relation** (the R1→R2 regression test) — rel 500 →
  member Relation(600), rel 600 empty. Output: new rel references new
  rel's new id.
- **Self-loop relation** — rel X → member Relation(X). Output: new rel
  references its own new id.
- **Mixed-type interleaved members** — relation with members in order
  `[Node, Way, Node, Relation, Way]` to stress phase-specific scatter.
- **Duplicate refs in a single way** — way with refs `[A, B, A]`,
  positions must survive scatter-back.
- **Custom `--start-id` overlapping old IDs** — e.g. `start_node_id=5`
  when input has nodes 3..10. No self-collision.
- **Orphan members** — relation member referencing an id not in input.
  Preserve current behavior (`unwrap_or(old_id)`) for compatibility
  with the in-memory path and osmium. Document as intentional.
- **Negative node id rejected in external mode** — must error,
  not silently corrupt.
- **Empty input** (header only), **no ways**, **no relations**,
  **zero-member relation**, **single-ref way** — smoke tests for blob
  boundary / pass-skip logic.
- **Sortedness of output by new id per type** — assert explicit type-by-
  type monotonicity rather than relying solely on the header flag.

Existing suite already covers back-ref relations, tag preservation,
sorted-header-flag check, and basic node/way/relation roundtrips;
don't duplicate.

---

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
   scaffolding into `renumber_external.rs`? ✅ **Resolved 2026-04-11** —
   extracted to `src/commands/external_radix.rs` before implementation
   started. Contains `ScratchDir` (with a `name` parameter so callers
   distinguish `external-join` from `renumber-external` scratch dirs),
   `BucketWriters`, `NUM_BUCKETS`, `BUCKET_BUF_SIZE`, and
   `advise_dontneed_file`. ALTW payload types (`CooPair`,
   `ResolvedEntry`, `load_coo_bucket_into`, `MAX_NODE_ID`) stayed in
   `external_join.rs` since they are join-specific and not shared
   scaffolding. `renumber_external.rs` will define its own
   `(old_id, new_id)` pair type and its own id-range partitioning
   constants. Verified via `brokkr check` + ALTW external-index
   end-to-end on Denmark.
3. **How does osmium renumber handle planet?** ✅ **Answered** — see
   "Prior art: osmium renumber" section below. Summary: osmium is
   in-memory-only, explicitly documented as "needs >32 GB RAM for
   planet." No external-join prior art to copy; our design is genuinely
   novel relative to the reference implementation, and osmium's
   upstream position is that planet-scale renumber requires a
   fat-memory machine. Also validates our two-pass relation handling,
   sorted-input requirement, and `--start-id` interface.
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

## Prior art: osmium renumber

Research pass against `research/libosmium/` and `research/osmium-tool/`
(2026-04-11, Opus Explore agent) to understand what the reference C++
implementation does before we commit to our external-join design. Full
findings below, with source citations.

### Headline: osmium's architecture is in-memory-only, and upstream explicitly scopes it out of planet.

`osmium-tool/src/command_renumber.cpp` (443 LoC) and `command_renumber.hpp`
(149 LoC) hold the entire implementation. **`command_renumber.cpp` does
not include a single header from `osmium/index/`** — the renumber command
deliberately does not use any of libosmium's pluggable map backends
(`flex_mem`, `dense_mmap_array`, `sparse_mmap_array`, the ones that `osmium
add-locations-to-ways` and `create-locations-index` lean on). Instead it
defines a bespoke `id_map` class, one per object type, held in an
`osmium::nwr_array<id_map>`.

The `id_map` data structure is a hybrid:

```cpp
std::vector<osmium::object_id_type> m_ids;          // 8 bytes each
std::unordered_map<osmium::object_id_type, osmium::object_id_type> m_extra_ids;
osmium::object_id_type m_start_id = 1;
```

The vector's index *is* the new ID (minus `m_start_id - 1`); the overflow
map catches referenced-but-missing IDs that can't be appended in sorted
order. The design assumes a sorted input file so most IDs can simply be
appended to `m_ids` in one pass. Lookups are `std::lower_bound` on `m_ids`
with `m_extra_ids` as a fallback.

**Per-entry cost: 8 bytes for the common case, ~48–56 bytes for entries
that land in `m_extra_ids`.** `m_extra_ids` is expected to be small on a
self-contained planet file (few referenced-but-missing IDs in practice),
so the floor is essentially `8 bytes × (node_count + way_count +
relation_count)` per invocation.

The `man/osmium-renumber.md` page makes the planet-scale position
explicit (lines 110–115):

> *"**osmium renumber** needs quite a bit of main memory to keep the
> mapping between old and new IDs. It is intended for small to medium
> sized extracts. You will need more than 32 GB RAM to run this on a
> full planet. Memory use is at least 8 bytes per node, way, and
> relation ID in the input file."*

At planet scale (~10.3B objects), the 8-byte floor alone is ~82 GB
before any `m_extra_ids` overhead. The 2020 manpage cites Germany
(~600M objects, ~5% of planet) at 7 GB / 3 minutes. Naive scale:
~135 GB and ~60 minutes for planet.

**The direct conclusion for pbfhogg:** if we want planet renumber to
fit on a 32 GB host, we cannot copy-and-adapt osmium's architecture.
Upstream's own framing is that planet requires a fat-memory machine.
Our external-join design is novel engineering relative to the reference
implementation, not a port — and that's fine, because osmium has
explicitly chosen not to solve this problem.

### No external-join infrastructure to reuse.

One hope going into the research was that `osmium renumber --index-directory`
might be doing some form of disk-backed join. It isn't. The `--index-directory`
flag persists each `id_map` to a simple binary file (`nodes.idx`, `ways.idx`,
`relations.idx`) at the end of the run, for **cross-file consistency** (so a
follow-up invocation on a matching `.osc` picks up the same IDs). On read,
the files are mmap'd briefly, then the contents are immediately copied into
a fresh `std::vector` in memory via a `push_back` loop. They are not
queried from disk during the run and they are not partitioned.

**There is no sort-then-join, no radix bucketing, no merge-join, no
streaming, no external query path anywhere in command_renumber.cpp.** The
full mapping lives in RAM for the duration of the run.

This matters because our design re-uses ALTW's `ScratchDir` /
`BucketWriters` / 256-bucket radix partitioning pattern from
`src/commands/external_join.rs`. There is no analogue to that pattern
in libosmium's renumber code, and no analogue anywhere else in libosmium
either — the index backends in `include/osmium/index/map/` are point-lookup
caches over dense-ish ID space (`flex_mem`, `dense_file_array`,
`sparse_file_array`, etc.), not partitioned-bucket external joins. They
solve a different problem: node-location maps for operations like
`add-locations-to-ways`, not the symmetric old→new renumber map.

### Two-pass relation handling: confirmed conventional.

This is the most reassuring cross-check. Our design calls for a
relations-only Pass R1 (assign new IDs, build the in-memory relation_map)
followed by a full Pass R2 (merge-join node refs against node_map buckets,
way refs against way_map buckets, relation member refs against the in-memory
relation_map). The motivation was forward-reference handling: a relation
can reference a higher-ID relation, and in a sorted PBF the target hasn't
been assigned a new ID yet when the referring relation is first visited.

`osmium-tool/src/command_renumber.cpp:380–403` does exactly the same
thing. First pass (`:380–383`) calls `read_relations(m_input_file,
&m_id_map(relation))` with `osmium::osm_entity_bits::relation` as the
reader filter, which skips node and way blobs via entity filtering and
walks only relations. Second pass (`:389–403`) walks the full file and
calls `renumber(buffer)` on each buffer, which knows that all relation
IDs are already assigned by the time it needs them. The first pass is
skipped entirely in single-pass-mode when relations aren't being
renumbered (e.g. `-t node -t way` on the CLI).

**Confidence +1 on our two-pass relation design.** The reference
implementation does exactly this, it's the conventional shape, and we
shouldn't waste time looking for a forward-ref buffering alternative.

### Sorted-input requirement: also confirmed conventional.

osmium uses `osmium::handler::CheckOrder` (`command_renumber.hpp:106`,
`command_renumber.cpp:262,268,279`) and throws on unsorted input. The
manpage (`man/osmium-renumber.md:21–24`) says explicitly:

> *"This command expects the input file to be ordered in the usual way:
> First nodes in order of ID, then ways in order of ID, then relations in
> order of ID. Negative IDs are allowed, they must be ordered before the
> positive IDs."*

Users are expected to pre-sort with `osmium sort` if they have unsorted
input. pbfhogg should follow suit — `renumber_external` keeps the
existing `require_sorted` check and documents "pre-sort with pbfhogg
sort" as the workflow for unsorted input. Open Question #6 in this doc
resolves to "no, we don't build unsorted support into renumber."

### `--start-id` semantics: worth matching osmium's surface.

osmium's `-s / --start-id` flag accepts two forms (`command_renumber.cpp:149–163`,
`:189`):

- Single integer: `-s 100` → all three types start at 100.
- Three comma-separated integers: `-s 1,100,-200` → nodes 1, ways 100,
  relations start at −200 and count downward.

Negative start IDs trigger a "count downward" mode (`command_renumber.cpp:61–66`):

```cpp
if (m_start_id < 0) {
    return -id + m_start_id + 1;
}
return id + m_start_id - 1;
```

Our current `RenumberOptions { start_node_id, start_way_id,
start_relation_id }` is functionally equivalent to osmium's three-tuple
form. The negative-countdown mode is a bonus osmium feature we don't
strictly need for correctness, but matching it costs ~5 LoC and gives
users drop-in compatibility with osmium's CLI. Worth adding during the
refactor since we're touching the code anyway.

### Bonus finding: osmium's apply-changes does NOT blob-passthrough.

Not strictly a renumber topic, but the research pass answered a
long-standing question about why pbfhogg's `apply-changes` is ~15× faster
than osmium's on planet-scale workloads. The answer: osmium decodes and
re-encodes **every** blob.

`osmium-tool/src/command_apply_changes.cpp:278–379` runs the base input
through `osmium::io::Reader` (full decode of every buffer) and every
object through a `std::set_union` merge against the sorted change buffer,
piped to `osmium::io::Writer` (full re-encode of every object). There is
no "if this blob doesn't touch any changed IDs, pass the raw bytes
through" optimization anywhere in the code path. For a typical daily diff
where ~94% of blobs contain zero changed IDs, pbfhogg's blob-passthrough
skips PBF zlib decode + re-encode on 94% of the input — which is exactly
where the ~15× speedup comes from, and it's a strict algorithmic
improvement over the upstream design.

This is documented here rather than in `notes/` elsewhere because the
finding was a by-product of the renumber research. Worth remembering if
anyone ever asks "why is pbfhogg so much faster at apply-changes" — it's
not tuning, it's an architectural difference that osmium has not
implemented.

### Bonus finding: osmium `derive-changes` has a real bug at `command_derive_changes.cpp:184`.

README's cross-validation section has long documented that "osmium's
derived OSC loses 1243 delete directives" on our Denmark cross-validation
run. The research pass traced the failure to a specific buggy line.

`osmium-tool/src/command_derive_changes.cpp:176–192` is a dual-iterator
merge walk over two sorted OSM files:

```cpp
while (it1 != end1 || it2 != end2) {
    if (it2 == end2) {
        write_deleted(writer, *it1);
        ++it1;
    } else if (it1 == end1 || *it2 < *it1) {
        writer(*it2);
        ++it2;
    } else if (*it1 < *it2) {
        if (it2->id() != it1->id()) {            // <-- BUG
            write_deleted(writer, *it1);
        }
        ++it1;
    } else { /* *it1 == *it2 */
        ++it1;
        ++it2;
    }
}
```

The guard on line 184 compares raw `int64` IDs. OSM IDs are unique
**only within a type**, not across types. When the merge walks past a
type boundary — e.g. `it1` is on a deleted node, `it2` is parked on the
first way of the new file (because all common nodes have been visited) —
the comparison `*it1 < *it2` evaluates TRUE (because libosmium's
`OSMObject::operator<` orders `(type, sign(id), |id|, version, ...)` so
node < way regardless of ID value). We enter the `else if` branch, and
**any coincidental numeric ID match between the deleted node and the
next way silently drops the delete**.

At planet scale with tens of millions of deletes and the way ID space
heavily overlapping the node ID space, 1243 false-negative deletes is
entirely plausible — it's roughly the number of deleted objects whose
numeric IDs happen to coincide with another-type objects at the specific
merge-walk boundary positions.

**The fix** is to compare `(it2->type(), it2->id()) != (it1->type(),
it1->id())` instead of just `id()`. pbfhogg's `diff --format osc` must
be using the correct tuple comparison, which is why our roundtrip test
passes and osmium's doesn't.

**This is a real upstream bug** worth filing against osmium-tool in a
future session. For now it's documented here, with the commit history
detail that a 2018 fix (commit `3edb895`) corrected `write_deleted` to
emit the right object type but didn't revisit the merge-walk comparison.
The bug is older than that fix and has been present in every released
version since.

### Takeaways for the external-join refactor

1. **Our architecture is novel relative to osmium.** No prior art to
   copy. osmium is in-memory-only, explicitly scoped out of planet.
   We're solving a problem the reference implementation has chosen
   not to solve. That's the right framing to carry into the
   implementation and the eventual README writeup.
2. **Two-pass relations: ship as designed.** Exactly what osmium does,
   same reasoning, same structure. Confidence level on that call is
   now higher.
3. **Sorted-input requirement: ship as designed.** Don't build unsorted
   support; users pre-sort via `pbfhogg sort`. Resolves Open Question #6.
4. **`--start-id` interface: matches osmium's surface.** Cheap to add
   the negative-countdown mode during the refactor for drop-in CLI
   compatibility.
5. **`ScratchDir` / `BucketWriters` extraction (Open Question #2):**
   still an open decision, but the research confirms there's nothing
   to reuse from libosmium — the extraction is purely within
   pbfhogg's own codebase. Decide based on pbfhogg's internal
   architecture, not cross-project concerns.
6. **`apply-changes` blob passthrough is a strict improvement over
   osmium.** Structural speedup, not a measurement artifact. Preserve
   that code path during any future refactors.
7. **`derive-changes` upstream bug is real and filed (as TODO in
   README's cross-validation section).** Future session work.

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
