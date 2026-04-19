# time-filter optimization

`brokkr time-filter --dataset <X>`: filter a sorted PBF to a snapshot
at a cutoff timestamp. Designed for history PBFs (pick the latest
version per (kind, id) with `timestamp <= cutoff`, drop if
`visible=false`); degrades to a per-element timestamp filter when the
input is a snapshot (all our current datasets).

Active code: [`src/commands/time_filter.rs`](../src/commands/time_filter.rs).

## Architecture

Dispatched on `header.has_historical_information()`:

- **History input** (`time_filter_history`): sequential
  pending-group state machine on the 3-stage pipelined reader
  (`for_each_pipelined`). Parallel decode, sequential
  state-machine + BlockBuilder on the callback thread. Version
  selection needs cross-element peek, and versions of one
  (kind, id) can straddle blob boundaries, so trivial per-block
  parallelism is unsafe.

- **Snapshot input** (`time_filter_snapshot`): parallel per-block.
  Each (kind, id) appears exactly once in a sorted snapshot PBF,
  so blocks are independent. Same shape as tags-filter
  single-pass: `for_each_primitive_block_batch` on
  `into_blocks_pipelined` + `batch.par_iter().map_init(
  BlockBuilder::new, ...)`. Workers iterate elements by
  reference, drop elements with `timestamp > cutoff` or
  `visible=false`, write survivors into a local BlockBuilder via
  `ensure_*_capacity_local` / `flush_local`. Consumer drains
  `Vec<OwnedBlock>` in batch order; the writer's own rayon pool
  handles compression.

## Baselines

| Commit | Dataset | Mode | Wall | UUID | Notes |
|---|---|---|---|---|---|
| `1e00c3d` | japan 2.4 GB | `--bench 1` | **44.1 s** | `444823d8` | sequential `for_each`, pre-optimization |
| `3035115` | japan 2.4 GB | `--bench 1` | **37.0 s** | `6e767a67` | iter 1: `for_each_pipelined`, avg cores 2.4 |
| `f45189e` | japan 2.4 GB | `--bench 1 --force` | **7.1 s** | dirty | iter 2: parallel per-block, avg cores 20.2 |
| `f45189e` | europe 35 GB | `--bench 1` | **95.1 s** | `a5d77c9a` | iter 2 at scale, peak anon 20 GB |
| iter 3 | europe 35 GB | `--bench 1` | **94.7 s** | `8b676229` | budgeted batch 128 MB, peak anon **18.1 GB** (-10 %) |
| iter 4 (pool landed) | europe 35 GB | `--bench 1 --force` | **94.3 s** | dirty | Vec pool + pre-grow 512 KB, peak anon **18.3 GB** (no-op vs iter 3) |
| iter 5 (pool works) | europe 35 GB | `--bench 1` | **92.6 s** | `6683cb05` | thread_local BlockBuilder; peak anon **16.9 GB** (-15.5 % vs iter 2); alloc churn -87 % |
| iter 6 (reverted) | europe 35 GB | `--bench 1 --force` | 112.2 s (+21 %) | dirty | raw-frame passthrough + pread workers - **regressed**, reverted |

Throughput at iter 2: ~370 MB/s input. `writer_reorder_high_water`
jumped 4 → 64 (compression pool saturated).

## Instrumentation

- End-of-run counters (cheap, always-on):
  `timefilter_versions_seen`, `timefilter_versions_before_cutoff`,
  `timefilter_elements_written`, `timefilter_dropped_deleted`,
  `timefilter_dropped_no_snapshot_version`,
  `timefilter_is_history_path`.
- Phase markers: `TIMEFILTER_HISTORY_START/END`,
  `TIMEFILTER_SNAPSHOT_START/END`.
- `#[hotpath::measure]` on `time_filter`, `time_filter_history`,
  `time_filter_snapshot`, `process_snapshot_batch`,
  `filter_block_snapshot`, `flush_group`, `write_owned_element`,
  `clone_owned_element`.
- **Do not add per-element `Instant::now()` timers in the callback**:
  tried it and the time-source alone doubled Japan wall from 37 s to
  73 s (344 M elements). The committed counters + hotpath attributes
  are the right shape; per-function breakdown comes from
  `brokkr time-filter --hotpath`.

## Alloc profile (japan iter 2, UUID `fed75758`)

48.7 GB allocated, 54.3 GB deallocated across the run. Exclusive alloc
bytes by function:

| Function | Calls | Avg | Total | % |
|---|---:|---:|---:|---:|
| `block_builder::take_owned` | 37,775 | 506.8 KB | **18.3 GB** | **75.5 %** |
| `parse_and_inline_with_scratch` | 37,858 | 114.9 KB | 4.1 GB | 17.2 % |
| `writer::frame_blob_into` | 30,640 | 43.2 KB | 1.3 GB | 5.2 % |
| `block_builder::add_node` | 233 M | 1 B | 250 MB | 1.0 % |
| `block_builder::add_way` | 795 K | 322 B | 245 MB | 1.0 % |
| `filter_block_snapshot` | 37,775 | 703 B | 25 MB | 0.1 % |

`take_owned` dominates alloc: every BlockBuilder finalization produces
a fresh `Vec<u8>` for the serialized block + indexdata, ~500 KB each,
37 K per Japan run. These Vecs are the same ones whose high-water
retention drives the 20 GB anon peak at Europe.

## Iter 3 notes (2026-04-19)

- **Budgeted batch landed.** `for_each_primitive_block_batch_budgeted`
  with a 128 MB cap on decoded bytes per batch. Europe peak anon 20 GB
  -> 18.1 GB (-10 %), wall unchanged at 95.1 s. Tightening to 32 MB
  *regressed* anon (back to ~20 GB) because the pipelined reader's
  decode-ahead expanded to compensate for slower batch consumption.
  128 MB is the sweet spot for this code shape; below that the
  allocator / decode-ahead takes up the slack.

- **`mallopt(M_ARENA_MAX, 2)` does NOT work here.** renumber_external
  uses it to drop planet peak anon from ~26 GB to <1 GB. Tried the
  same one-liner in time_filter. Measured regression on Europe
  `--bench 1`: wall 95.1 s -> 160.4 s (+69 %), peak anon 20 GB ->
  24.8 GB (+24 %), avg cores 20.4 -> 14.1. The reason is the command
  class: renumber workers do low-alloc wire-format splice, time-filter
  workers do allocation-heavy full BlockBuilder re-encode. With 2
  arenas the malloc lock contention dominates the fragmentation win.
  **Do not re-attempt** - the pin comment in `time_filter.rs` carries
  the full measurement.

- **`parse_and_inline_with_scratch` audit resolved.** Explore agent
  traced the scratch lifecycle through `src/read/pipeline.rs:178-195`
  (thread-local ST_SCRATCH / GR_SCRATCH Vecs per decode task,
  `.clear()` + capacity retention between blobs). The 4.1 GB reported
  by alloc mode is per-worker capacity held across the run, not
  per-call churn. Previous "opportunity #3" in this doc was based on
  a misreading - **strike it**; the reduction from 829 MB -> 48 MB
  claimed in TODO.md *is* wired into the snapshot path.

## Iter 4 notes (2026-04-19): Vec pool lands, doesn't pay off

Landed the full pool infrastructure over two commits:
[`src/write/buf_pool.rs`](../src/write/buf_pool.rs) with a bounded
`Mutex<Vec<Vec<u8>>>` + RAII-adjacent get/put API (instrumented with
hit/miss, put/capacity, len counters),
[`BlockBuilder::take_owned_swap`](../src/write/block_builder.rs) as
a sibling to `take_owned` that `std::mem::replace`s a caller-provided
`Vec<u8>` in for the next encode cycle, and
[`PbfWriter::write_primitive_block_owned_pooled`](../src/write/writer.rs)
that returns `block_bytes` to the pool inside the rayon compression
closure's tail. Wired end-to-end through
`time_filter_snapshot -> process_snapshot_batch -> filter_block_snapshot`
with local `flush_local_pooled` + `ensure_*_capacity_pooled` helpers.

**Measurement:** Europe wall 95.1 s -> 94.3 s (within noise); peak
anon 20.0 GB (iter 2) / 18.1 GB (iter 3 budgeted batch alone) ->
18.3 GB (iter 4 pool + budgeted batch). The pool is doing its job
mechanically (Europe: 522 K gets, 87 % hit rate, 0 puts dropped) but
**does not move the Europe RSS needle** over iter 3's budgeted batch.

**Root cause, diagnosed via pool counters:** `par_iter().map_init(
BlockBuilder::new, ...)` creates a fresh `BlockBuilder` **per rayon
task, not per thread**. Each `BlockBuilder` processes roughly one
block, so its first `encode_block` always allocates `encode_buf` from
`cap=0` - the pool-sized `swap` installed after that encode is for
the *next* call that never comes (the task ends). Average put
capacity matches block size (~140 KB) rather than the pre-grown
target, confirming the diagnosis: pool Vecs get to BlockBuilder, but
BlockBuilder discards them unused because the first and only encode
already finished.

**The pool stays landed** - it's correct, tested (three unit tests
plus per-run counters), and unblocks iter 5. Cost when unused by
longer-lived callers is zero (`Arc` clones and bounded mutex touches
only fire in the snapshot path). The next iteration is the lever
that actually pays the pool off: make `BlockBuilder` persistent
across rayon tasks.

## Iter 5 notes (2026-04-19): pool pays off via thread_local BlockBuilder

Replaced `batch.par_iter().map_init(BlockBuilder::new, ...)` with
`batch.par_iter().map(|block| SNAPSHOT_BB.with_borrow_mut(...))`
where `SNAPSHOT_BB` is a module-scope
`thread_local!<RefCell<BlockBuilder>>`. Rayon reuses a fixed pool of
worker threads across successive `par_iter()` calls, so a
thread-local persists the same `BlockBuilder` across all batches
processed by that thread. `take_owned_swap` now installs a pool-sized
swap whose capacity survives to the next encode on the same
BlockBuilder instead of dying with the per-task one.

**Pool counter change (Japan):**

|                         | iter 4  | iter 5  |
|-------------------------|---------|---------|
| gets_total              | 43,035  | 43,035  |
| gets_hit                | 34,748  | 34,748  |
| avg get capacity        | 136 KB  | 576 KB  |
| avg put capacity        | 136 KB  | 576 KB  |
| avg put len (block size)| 136 KB  | 140 KB  |

**Alloc profile change (Japan, UUID `fed75758` -> `86db6ef6`):**

| Function                              | iter 3 (no pool) | iter 5 (pool + TLS) |
|---------------------------------------|------------------|---------------------|
| `take_owned` / `take_owned_swap`      | 18.3 GB (75 %)   | **109 MB (1.7 %)**  |
| `parse_and_inline_with_scratch`       | 4.1 GB (17 %)    | 4.4 GB (70 %)       |
| `frame_blob_into`                     | 1.3 GB (5 %)     | 1.5 GB (24 %)       |
| Total allocated                       | 48.7 GB          | **~6.3 GB**         |

**Europe wall / RSS (UUID `6683cb05`):**

- Wall 94.7 s -> **92.6 s** (-2.2 % vs iter 3).
- Peak anon 18.1 GB -> **16.9 GB** (-15.5 % vs iter 2; -6.6 % vs
  iter 3).
- Avg cores 20.3 -> 20.5.

Planet extrapolation (naive linear, Europe 16.9 GB at 35 GB input
-> planet 92 GB at ~45 GB anon). Still over the 27 GB host ceiling,
but comfortably within striking distance of opportunity #2 (raw blob
passthrough for all-survive blocks) or finer in-flight-bytes
tuning.

## Iter 6 notes (2026-04-19): raw-frame passthrough *reverted*

Attempted raw-frame passthrough for all-survive blobs following the
`extract --strategy simple` pattern in
[`src/commands/extract/common.rs`](../src/commands/extract/common.rs):
header-only schedule scan + pread-from-workers + scan-first-then-
dispatch, with workers emitting `WorkerOutcome::Passthrough` (frame
offset + size) when every element has `timestamp <= cutoff &&
visible`. Consumer preads the raw frame for passthrough items and
writes via `PbfWriter::write_raw_owned`, skipping BlockBuilder
re-encode and frame_blob_into compression entirely.

**Result on the benched workload: net regression, reverted.**

|                        | japan          | europe          |
|------------------------|----------------|-----------------|
| wall vs iter 5         | 6.8 s -> 8.1 s | 92.6 s -> 112.2 s |
| all-survive blobs      | 309 / 43,035   | 3,291 / 522,168  |
| passthrough rate       | **0.72 %**     | **0.63 %**      |

**Two independent reasons it lost:**

1. **Passthrough rate is a floor-level fraction at this cutoff**
   (2024-01-01 on a 2026-02 snapshot). Elements within a blob share
   nearby IDs, i.e. similar creation eras, but their *last-edit*
   timestamps are independent enough that any single post-cutoff
   edit disqualifies the whole blob. Break-even was ~14 %
   passthrough (scan cost vs savings); we got 0.7 %. Mathematically:
   with 77 % element-survive rate and 8,000 elements/blob, P(all
   survive) is effectively zero unless time-of-edit is strongly
   correlated across nearby IDs - and for OSM snapshots with active
   recent editing across the graph, it is not.
2. **The I/O architecture change itself lost wall.** iter 5 uses
   `ElementReader::into_blocks_pipelined` (3-stage pipeline: IO ->
   rayon decode -> reorder, with async I/O overlapping decode).
   iter 6's pread-from-workers pattern does synchronous pread per
   blob per worker. Even with passthrough savings at 0 %, the I/O
   swap alone cost wall on Europe. The pipelined reader is a
   well-tuned piece we should not give up without a compensating
   win.

**When passthrough *would* pay:** very recent cutoffs (e.g.,
"filter out edits from the last week" on a multi-year-old
snapshot) where many low-ID blobs contain exclusively pre-cutoff
edits. At that regime passthrough rate could push into the tens of
percent and clear the break-even bar. Not the workload anyone
benches us on, and not something the current CLI configures, so
there's no justification to carry the code.

**Lesson:** both of the notes/raw-group-passthrough.md pattern's
lessons apply in reverse here. (a) The shadow-counter methodology
would have caught this before the refactor - we could have
instrumented iter 5 to count all-survive blobs without switching
I/O paths and seen the 0.7 % fraction directly. (b) Even if
passthrough had paid, landing it on a different I/O architecture
than the winning one was a hidden cost. Next time: measure first
with instrumentation on the existing architecture before rewriting
the I/O path.

Reverted in-place; no commit landed iter 6.

## Remaining opportunities (ranked)

### 1. Pool `take_owned` output Vecs - LANDED iter 4

### 2. Persistent BlockBuilder across rayon tasks - LANDED iter 5

Biggest alloc target (75 % at Japan = 18.3 GB of churn; extrapolating
Europe at ~4× element count gives ~70 GB of churn and explains the
18 GB peak anon after iter 3). Lifecycle:

- Worker: `BlockBuilder::take_owned()` allocates `Vec<u8>` for
  serialized block bytes + indexdata `Vec<u8>` + optional tagdata.
- Consumer: writes via
  `PbfWriter::write_primitive_block_owned(bytes, index, tagdata)`
  at `src/write/writer.rs:616`. Writer captures into a rayon closure
  (line 640), calls `frame_blob_into()` (line 642) which **clones**
  the bytes into a fresh `FramedBlobParts` (`lines 1160-1164`). The
  original Vecs are then dropped when the closure scope ends at
  `line 666` (pipelined) or `line 674` (sync). That drop point is
  where a pool would intercept them.

No existing pool for this lifecycle (Explore agent Q1, 2026-04-19).
Closest adjacent pattern is `DecompressPool` + `PooledBuffer` at
`src/read/blob.rs:46-93`, which uses RAII-return-on-drop. The
writer-side receiving end is also unhooked today (Q3) - any pool
design needs to add either a completion callback on
`OutputSink::write_chunk()` or a drop-to-channel wrapper on the Vecs
entering the closure.

Primary motivation: **anon RSS for the planet-scale run, not wall
time**. Cuts the 18 GB Europe peak further and unblocks planet on
the 27 GB host. Wall impact is secondary (allocator fast-paths
handle ~500 KB blocks well already). Landing shape: modest
multi-commit arc - pool primitive, BlockBuilder emit-into-pool hook,
writer drop-point hook, plumb through `time_filter_snapshot`.

### 3. Raw blob passthrough for all-survive blocks - ATTEMPTED + REVERTED iter 6

See the iter-6 post-mortem above. **Measured 0.63 % passthrough on
Europe at cutoff 2024-01-01 - far below the ~14 % break-even bar**.
The theoretical correlation between adjacent IDs (nearby elements
share editing eras) does not survive contact with an active graph:
any single post-cutoff edit in a blob disqualifies the whole blob,
and at 8,000 elements/blob and realistic edit activity, P(all
survive) collapses. Passthrough is viable only at very recent
cutoffs (e.g. "filter out edits from the last week"); not a common
workload, no justification to carry the code.

### 2. Parallel history-input path

Sequential state machine, avg cores 2.4 at iter 1. No history PBFs
currently in the dataset inventory so the wall doesn't show up in
benches - keeping this deferred until a real workload lands.

Shape: workers decode + run per-block version selection emitting
`(prefix_complete_blocks, head_partial_group, tail_partial_group)`.
Consumer stitches blob N+1's head with blob N's tail when they match
(kind, id); writes the stitched winner as its own group.

### 4. Blob-level timestamp range index

Blob index v1 carries `kind/min_id/max_id/count/bbox` - no timestamp
range. With a timestamp range per blob, the scheduler could:

- Drop blobs entirely above the cutoff without decompressing them.
- Mark blobs entirely below the cutoff (and confirmed all-visible) as
  raw-passthrough candidates without per-element scanning.

Format bump, coordinates with other commands that might use
timestamp metadata (merge-changes? extract history slices?). Big
surface-area change - only worth it if time-filter becomes a hot
command, and probably after (#4) since (#4) needs no format change.

### 5. Tune in-flight depth + mallopt variants

Smallest-scope post-iter-5 move. Two knobs sit above time-filter's
code and hold peak anon above what the pool + thread_local already
saved:

- **3-stage pipelined reader's decode-ahead buffer depth** and
  **writer's reorder / compression pool depth**. Both use defaults
  today. Shrinking either trades wall parallelism for peak RSS. On
  Europe iter 5 the writer's `reorder_high_water` reached 32; cut
  that cap to 8 and the writer holds far fewer in-flight compressed
  chunks. Similar thing on the reader side. Knob locations:
  [`src/read/pipeline.rs`](../src/read/pipeline.rs) for the decode
  pipeline capacity constants,
  [`src/write/writer.rs`](../src/write/writer.rs) for the reorder /
  permit pool.
- **`mallopt(M_ARENA_MAX, K)` with K > 2.** Iter 3 tried K=2 and
  regressed hard (wall +69 %, anon +24 %) because the malloc lock
  became the bottleneck. K=4 or K=8 might sit in the middle: fewer
  arenas than default (which the glibc heuristic chooses at runtime
  based on core count, typically ~16) but more than 2 so lock
  contention is manageable. One-line experiment, same single-commit
  shape as the failed K=2 attempt; bench Europe `--bench 1` each.

Expected win: low-single-digit GB of anon. Cheap to try. Do this
before any planet attempt regardless of whether we continue the arc.

### 6. Reader-side scratch cap

Targets the largest remaining alloc contributor after iter 5: per-
worker thread-local scratch Vecs in
[`src/read/pipeline.rs:178-195`](../src/read/pipeline.rs) that grow
to the max-block-size seen on each decode thread and never shrink.
Iter 5 alloc profile: `parse_and_inline_with_scratch` = 4.4 GB, 70 %
of total remaining alloc. The Explore agent's iter-4 Q2 confirmed
this is **retained capacity across the run**, not per-call churn -
so the hot-path cost is already minimal; the lever here is anon RSS
for planet, not wall.

Shapes to consider:

- **Smaller retention cap per thread.** Right now each worker's
  scratch grows to whatever the biggest blob it touches needs and
  stays there. A Vec::shrink_to with a modest ceiling (e.g. 256 KB)
  after each blob would bound retention at N_threads × 256 KB but
  re-alloc on bigger blobs.
- **Shared bounded pool instead of per-thread.** Same
  `BlockBufPool`-style pattern as iter 4, applied to the reader's
  scratch. Workers pull pre-grown scratch at the top of each decode,
  return after. Bounds aggregate retention to `POOL_CAPACITY *
  scratch_size` regardless of thread count. More refactor than the
  shrink-on-loop path; roughly 2-3 commits.

Do the shrink-on-loop version first; measure. If it lands close to
its target, the pool shape is overkill. If it regresses wall (extra
allocs on big blobs), fall back to the pool shape.

### 7. Adaptive passthrough via shadow counter (iter 5 path)

The iter-6 lesson in post-mortem form: measure the all-survive blob
fraction **on the existing I/O architecture** before investing in a
new one. Concrete shape:

- Add a cheap scan inside iter 5's
  [`filter_block_snapshot`](../src/commands/time_filter.rs) that
  does the `ts <= cutoff && visible` predicate as a separate pass,
  returns `(all_survive: bool, total)`. Counter at end of the phase
  reports `timefilter_all_survive_blobs / timefilter_total_blobs`.
- If reported fraction on a real workload is > ~20 %, re-investigate
  passthrough - but from the pipelined-reader path (via
  `raw_group_bytes` / `frame_raw_block` scaffolding in
  `src/write/raw_passthrough.rs`), not a pread swap. The iter-6
  I/O change was as big a loss as the low passthrough rate; any
  future attempt needs to stay on the winning I/O architecture.
- Narrow-cutoff workloads ("last week of edits on a 1-year
  snapshot") are where the fraction clears the break-even bar. We
  don't bench those today. This is a latent feature unlocked when a
  user lands one.

Cheap to land the counter (~20 lines). Measure first; decide
second.

## Relationship to other documents

- Hot-path & alloc methodology template: see the renumber_external
  arc (TODO.md "Active optimization plans" and
  [`src/commands/renumber_external/mod.rs`](../src/commands/renumber_external/mod.rs)).
  Same winning pattern as here - parallel decode, worker-parallel
  block work, consumer forwards OwnedBlocks to a writer with its own
  rayon compression pool.
- Per-block parallel pattern template: `tags_filter.rs` single-pass
  (`for_each_primitive_block_batch` + `par_iter().map_init(
  BlockBuilder::new, ...)`). The snapshot path here is a direct
  adaptation.
- The "don't chase raw passthrough without measuring" rule from
  `src/commands/tags_filter.rs` pass-2 worker applies to opportunity
  #4 above: measure the all-survive fraction before building the
  passthrough path.

## Planet

**Not running planet** until opportunity #1 lands. Europe peak anon
20 GB at 35 GB input → naive linear extrapolation to planet 92 GB is
~52 GB, which OOMs the 27 GB bench host. Pooling `take_owned` is the
blocker; everything else is follow-up.
