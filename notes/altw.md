# add-locations-to-ways: dense and sparse paths

`pbfhogg add-locations-to-ways --index-type dense|sparse`. The third
index type, `external`, is documented in
[`notes/altw-external.md`](altw-external.md) and its optimization arc
in [`notes/altw-optimization-history.md`](altw-optimization-history.md);
this file is dense / sparse only.

Code:
- [`src/commands/altw/mod.rs`](../src/commands/altw/mod.rs) (dispatch
  + pass 0 + pass 2 decode-all fallback).
- [`src/commands/altw/dense.rs`](../src/commands/altw/dense.rs).
- [`src/commands/altw/sparse.rs`](../src/commands/altw/sparse.rs).
- [`src/commands/altw/passthrough.rs`](../src/commands/altw/passthrough.rs)
  (pass 2 indexed path; the planet-recommended dispatch).

## Phases

Both dense and sparse share the surrounding pipeline:

1. **Pass 0** (`collect_way_referenced_node_ids`) -
   `parallel_classify_phase` with a single shared `IdSet`. Per-blob
   workers emit `Vec<i64>` of way refs; main thread unions them.
2. **Pass 1** (`build_node_index`) - diverges:
   - Dense: file-backed mmap (128 GB virtual, OS page cache),
     sequential blob walk on the main thread, then
     `tuples.par_iter().for_each` writes coords to mmap slots via
     `SharedDenseWriter`'s atomic stores.
   - Sparse: parallel-classify-phase + reorder-buffer build over a
     `BufWriter` to a temp file. Chunk layout: 256 IDs per chunk
     with `start_pad` to skip leading empty slots. Workers emit
     filtered (id, lat, lon) tuples per blob; consumer drains in
     seq order through the reorder buffer and runs a single
     chunk-streaming state machine over the merged stream.
3. **Optional rel-member scan**
   (`collect_relation_member_node_ids`, fires when
   `keep_untagged_nodes=false`) - `parallel_classify_accumulate`
   with per-worker `IdSet`, merged at the end.
4. **Pass 2** - dispatches on `indexdata_present`:
   - `write_output_passthrough` (indexed input): two-phase header /
     data read, batches `BatchSlot`s, parallel decompress + parse +
     `process_block` per slot via `process_slot_batch`. Way refs
     resolve via inline `NodeIndex::get` in the per-block worker.
   - `write_output_decode_all` (`--force` on non-indexed input):
     `into_blocks_pipelined` + batch + `par_iter().map_init(
     BlockBuilder).collect()` + drain via `process_batch`. Same
     inline `NodeIndex::get` resolution.

## Status

| Path | Denmark | Japan | Europe | Planet |
|------|---------|-------|--------|--------|
| Dense | safe | safe | thrash, OOM | thrash, OOM |
| Sparse | safe | safe | I/O bound (52 GB mmap > cache) | n/a |
| External | safe | safe | safe | safe |

Dense / sparse are both fast at sizes where the index fits in RAM;
above that, neither is viable. External (separate document) is the
only path that survives at europe+ scale. The dense / sparse
optimizations below have made them competitive at small / medium
scale, but the structural ceiling above ~25 GB working set is
unchanged.

## Measured walls (commit 9c1c83e or later, plantasjen 2026-04-29)

Three baseline points across the optimization arc, denmark + japan:

| Dataset | Mode | `68806b0` (pre) | `29683ee` (parallel pass 1) | `8e0cef9` (inline pass 2) |
|---------|------|-----------------|------------------------------|----------------------------|
| Denmark | dense | 11.9 s | - | - |
| Denmark | sparse | 17.3 s | 15.6 s | **5.8 s** |
| Japan | dense | 51.6 s | - | - |
| Japan | sparse | 78.4 s | 71.7 s | **20.9 s** |

Sparse went from 1.5x slower than dense at japan to **2.5x faster**
than dense at japan (20.9 s vs 51.3 s). Dense has not been touched.

Per-phase profile at the final state (commit `8e0cef9`, japan
sparse, UUID `fa7e61ed`):

| Phase | Wall | Peak RSS | Avg cores |
|-------|------|----------|-----------|
| Pass 0 | 4.9 s | 1.5 GB | 5.4 |
| Pass 1 | 3.5 s | 1.9 GB | 6.3 |
| Rel-member scan | 4.2 s | 4.3 GB | 1.5 |
| Pass 2 | 8.3 s | 8.6 GB | 19.9 |

Counters at scale (`altw_referenced_node_ids` x 8 bytes is a good
proxy for the dense working set or sparse mmap touched-page set):

| Counter | Denmark | Japan | Europe |
|---------|---------|-------|--------|
| `altw_referenced_node_ids` | 49 M | 299 M | 3,617 M |
| `altw_relation_member_node_ids` | 25 K | 193 K | 10.6 M |
| Sparse temp file | 1.0 GB | 5.7 GB | ~52 GB |

## Findings

### Dense fails above ~25 GB working set

Touched mmap pages scale linearly with `altw_referenced_node_ids` x
8 bytes:

- Denmark: 49 M x 8 = 393 MB. Fits trivially.
- Japan: 299 M x 8 = 2.4 GB. Fits, 3.1 M pass-1 majflt indicates
  the page cache is already churning a bit.
- Europe: 3,617 M x 8 = 29 GB. Exceeds the 27 GB-free host.
  Catastrophic page-thrash: 12 M majflt in pass 1 (4m18s), 23 M
  majflt in 13 minutes of pass 2 before SIGKILL. 2.1 TB read off
  disk for 35 GB of input.

Architectural, not a tuning gap. Pre-pass-0 filtering already
restricts to way-referenced nodes; the working set IS those nodes
times 8 bytes. Above host free RAM, dense cannot work.

### Sparse pass 2 is global-locality-bound at scale

Sparse pass 2 is fast at small / medium scale (japan: 8.3 s wall,
avg cores 19.9, peak RSS 8.6 GB) and fails at europe scale (killed
at 11 min, 14.9 M majflt, 1.38 TB disk read for 35 GB input). The
failure is a working-set overflow, not a parallelism or instruction-
mix problem.

The sparse temp file is ~52 GB at europe (linear from japan's 5.7 GB
at 1/12 the data). The host has ~25 GB available page cache after
the application's RSS settles. Each way's nodes scatter across the
ID space, so each block's lookups land on pages spread across the
whole 52 GB index. With 25 GB cache and 52 GB total, ~50 % of
accesses fault to disk regardless of order.

The sort-by-id-or-offset trick converts random access into sorted
access **within a sort run**. At small scale (whole index in cache)
the sort is wasted overhead - inline lookup wins because the cache
absorbs everything. At europe scale, sorting per-block produces
short sorted runs whose pages are then evicted before the next
block's run can use them; the cross-block global access pattern
remains random. This was measured directly (see "Don't re-attempt"
below): per-block sorted resolve produced no measurable improvement
over inline at europe (both killed at the same wall, same disk read,
same majflt).

The serial pre-batch resolve (the v1 design) sorts globally across
the batch which gives the prefetcher a longer run, but capped pass 2
parallelism at avg cores ~4 - which alone slowed pass 2 ~5x at small
scale. Either way, total page faults at europe-scale are bounded by
"cache size vs working set", and sorting only changes the order in
which the faults happen.

The structural fix, if sparse should work at europe scale, is a
**smaller encoding** that fits the index in cache. Today's chunk
format wastes ~57 % at japan density (5.7 GB temp / 299 M nodes =
19 bytes/node, vs 8 byte minimum). A bitmap+packed encoding could
plausibly halve this, putting europe sparse close to or under
cache.

### Rel-member scan IdSet bloat scales hard

`collect_relation_member_node_ids` uses `parallel_classify_accumulate`
with one IdSet per worker. Per-phase anon delta during the scan:

- Denmark: +0.9 GB.
- Japan: +2.5 GB.
- Europe: +9.7 GB.

24 workers x ~400 MB per IdSet at europe. The mod.rs doc comment
claimed ~68 MB / worker; measurement disagrees by 6x. Linear
extrapolation: planet ~24 x ~3 GB = ~72 GB. Same shape that bit
tags-filter `--invert-match` (28.3 GB peak anon -> 7.0 GB after the
2026-04-28 migration); same fix template available.

This is independent of dense vs sparse choice and would be a planet
blocker on its own.

### What is NOT the bottleneck

The `par_iter().map_init(BlockBuilder).collect()` shape in pass 2.
Peak anon stays under 4 GB at every measured scale, including
europe before the dense mmap thrash dominated the sidecar profile.

The shape != root cause lesson holds; see commit `48685ba` (getid
add-referenced) and the tags-filter `9d41465` doc landing for two
prior incidents where this shape was suspected and measurement
ruled it out. The pass-2 ceilings come from index access patterns
(dense: anon working set, sparse: file-backed working set), not from
the rayon-collect pattern.

## Landed work

### Parallelize sparse `build_node_index_sparse` - landed `29683ee`

`parallel_classify_phase` + `ReorderBuffer` shape, mirroring the
time-filter snapshot migration (`83183fb`). Workers receive one
PrimitiveBlock each, filter by referenced node IDs, emit
`Vec<(id, lat, lon)>` in blob-internal ID order. Consumer drains in
seq order through a 64-slot reorder buffer and runs the existing
chunk-streaming state machine.

Result:

| Dataset | Pass 1 wall | Pass 1 cores |
|---------|-------------|--------------|
| Denmark | 2.2 s -> 1.16 s (1.9x) | 1.0 -> 5.8 |
| Japan | 10.7 s -> 3.47 s (3.1x) | 1.0 -> 6.3 |

Peak RSS unchanged. The "strictly increasing node IDs" precondition
is preserved by the ReorderBuffer drain order.

### Inline `NodeIndex::get` in pass 2 - landed `8e0cef9`

Removed the serial `resolve_batch_locations` pre-pass that capped
sparse pass 2 at avg cores ~4. process_block now takes &NodeIndex
directly; both dense and sparse use inline lookup. Reverted the
`process_slot_batch` / `process_slot_batch_dense` split into one
function. Removed `LocationLookup` enum, `LookupEntry` struct,
`decompress_slot_batch`, `SparseArrayIndex::byte_offset` and
`SparseArrayIndex::get_at_offset` (all dead with the resolve gone).

Result:

| Dataset | Pass 2 wall | Pass 2 cores | Total wall |
|---------|-------------|--------------|------------|
| Denmark | 11.2 s -> ~3 s | 4.2 -> 16+ | 17.3 s -> 5.8 s (2.98x) |
| Japan | 56.7 s -> 8.3 s | 4.1 -> 19.9 | 78.4 s -> 20.9 s (3.75x) |

Japan sparse went from 1.5x slower than japan dense to 2.5x faster.

## Remaining work

### 1. Migrate rel-member scan off `parallel_classify_accumulate`

**Why first:** real planet-scale blocker independent of index-type
choice. Affects every `keep_untagged_nodes=false` run, which is the
default. Whether or not we ever fix sparse at europe, this fires for
dense and sparse runs alike.

**Shape:** mirror the tags-filter way-deps migration (commit
`17b116c`). Replace `parallel_classify_accumulate` with
`parallel_classify_phase`: per-blob worker emits `Vec<i64>` of
relation member node IDs, main thread unions into a single shared
IdSet. Bounds memory to one IdSet plus per-blob transient vectors,
not N-workers x per-worker IdSet.

**Test surface:** existing CLI integration tests cover the
keep / drop semantics; set-union is commutative so the migration
preserves correctness by definition.

### 2. Sparse encoding redesign (optional, large investment)

**Status:** speculative. Land only if there is a real workload need
for sparse at europe-or-larger scale that isn't served by external.

**Goal:** shrink the sparse temp file below available page cache so
random pass-2 access stops thrashing. Today: ~19 bytes/node at japan,
extrapolating to ~50 GB at europe (cache is ~25 GB).

**Candidates:**

- 64-slot chunks instead of 256, smaller per-chunk overhead per node
  but more chunks. Net effect data-dependent; a quick prototype
  would say if this halves the file or just shifts bytes.
- Bitmap-per-chunk plus packed-entries layout. Each chunk stores a
  256-bit presence map (32 bytes) plus only the present (lat, lon)
  pairs, no sentinels. At japan's chunk density (~30 entries / 256
  slots) this would land near 8 bytes / node, ~2.4x shrink.
- Two-level chunk layout (super-chunk -> chunk -> slot) so very
  sparse super-chunks get 0 storage instead of N empty chunks.

**Constraint:** any redesign must keep Pass 1 parallelisable and
keep Pass 2's `NodeIndex::get` reasonably fast. A bitmap-and-pack
chunk requires `popcount(bitmap[..slot]) << 3` to find the byte
offset, vs today's `start_pad + slot << 3`. Cheap on modern CPUs
but per-call overhead matters at billions of lookups.

**Or:** just document that sparse is small / medium scale and keep
external as the planet-recommended path. (Current state.)

### 3. Per-batch parallel resolve (optional, lower priority)

**Status:** ranked low. Inline lookup at small / medium scale wins
on its own merits; the regime where global-locality sort would help
(europe sparse) is also the regime where sparse fundamentally
doesn't fit, and global sort can't change that.

**Shape:** the v1 `resolve_batch_locations` did global-batch sort
on a single thread; what was missing was parallelism. A version
that splits the sorted ref list into N chunks across rayon workers
would parallelise the scan while preserving global-locality. But
it only pays off at scales where the index doesn't fit cache - at
which point sparse is already failing for the working-set reason,
not the access-order reason. Leaving this here as a record; not
worth pursuing without the encoding fix above.

## Don't re-attempt

- **`parallel_classify_accumulate` with per-worker IdSet at scale.**
  See doc caution at `src/scan/classify.rs:300-317`. The rel-member
  scan above is an open example.
- **Dense at planet without 30+ GB free RAM.** Page-thrashing is
  architectural, not a tuning gap. External or sparse, not dense.
- **Per-block sorted resolve as a sparse-pass-2 fix at europe scale**
  (commit `d9edb5f`, reverted). Each block's refs scatter across the
  whole ID space, so per-block sort gives the prefetcher only short
  runs that are evicted before the next block needs them. Measured:
  identical kill point as inline (1.38 TB read, 14.9 M vs 15.3 M
  majflt), and adds ~20 % overhead on japan pass 2 from HashMap
  construction. Different `process_block` lookup mechanism, same
  cache-miss-bound fate.
- **Treating shape as the diagnosis.** The
  `par_iter+collect+drain` pattern was the suspect for sparse
  pass 2 thrashing - measurement instead pointed at single-thread
  resolve (the `8e0cef9` win) and at index-vs-cache size (the
  europe failure). Bench first, find the actual peak phase, only
  then rewrite.

## Cross-references

- [`notes/altw-external.md`](altw-external.md): the third index
  type, structurally different (external join via double radix
  permutation), already optimized.
- [`notes/altw-optimization-history.md`](altw-optimization-history.md):
  the external optimization arc.
- [`src/scan/classify.rs`](../src/scan/classify.rs):
  `parallel_classify_phase` (streaming, single shared state) vs
  `parallel_classify_accumulate` (per-worker state, merged at end).
  The choice criteria at lines 300-317 are load-bearing.
- Migration template precedents: time-filter snapshot (commit
  `83183fb`), tags-filter way-deps (`17b116c`), `cat --clean`
  (`b347c0a`), `check --ids` streaming (`516129e`).
