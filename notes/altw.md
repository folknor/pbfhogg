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

**Re-verified after the 5-item optimization arc (commit `e63d0b6`):
europe sparse still OOMs at pass 2.** UUID was a `--bench --force`
dirty run, but the per-phase profile is conclusive:

  Pass 0:        65 s (wire-only scan working).
  Pass 1:        76 s (52.8 GB sparse temp file written).
  Rel-member:    0.7 s (planet blocker fixed by the new shape).
  Pass 2:    9 m 56 s -> SIGKILL (OOM).
  Pass 2 majflt:        19.7 M.
  Pass 2 disk read:     1.73 TB for 35 GB input.
  Pass 2 avg cores:     2.9 (vs ~21 expected; bound by page faults).

Today's run actually had MORE majflt than yesterday's pre-arc
baseline (19.7 M vs 14.9 M). The descriptor-first pipeline +
parallel writer add concurrent workers (peak threads 65 vs 26
before), all of them page-faulting on disjoint regions of the
52 GB mmap. More parallelism IS NOT a fix for working-set
overflow - it makes the thrash *more* parallel.

The five-item optimization arc therefore landed exactly the wins
the doc predicted in advance: small / medium 31% faster, the
rel-member planet blocker bounded - and made no progress on the
sparse pass-2 europe ceiling, which is structural and only
addressable by **shrinking the encoding** or **changing the
access pattern** (see "Remaining work" below).

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

### Rel-member scan IdSet bloat scales hard (fixed)

Was: `collect_relation_member_node_ids` used
`parallel_classify_accumulate` with one IdSet per worker. Per-phase
anon delta during the scan:

- Denmark: +0.9 GB.
- Japan: +2.5 GB.
- Europe: +9.7 GB.

24 workers x ~400 MB per IdSet at europe. The mod.rs doc comment
claimed ~68 MB / worker; measurement disagreed by 6x. Linear
extrapolation: planet ~24 x ~3 GB = ~72 GB. Same shape that bit
tags-filter `--invert-match` (28.3 GB peak anon -> 7.0 GB after the
2026-04-28 migration). Independent of dense vs sparse choice; would
have been a planet blocker on its own.

Migrated to `parallel_classify_phase` in commit (this commit);
see "Landed work" below.

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

### Sparse rank-indexed flat layout - landed (this commit)

`build_node_index_sparse` rewritten to use a rank-indexed flat
encoding instead of the chunk + start_pad scheme:

  IdSet::build_rank_index() (one-time, ~100 MB at planet) ->
  set_len(referenced.total_count() * 8) on a temp file ->
  MmapMut + raw `*mut u8` shared with workers -> workers extract
  (id, lat, lon) tuples via scan::node::extract_node_tuples
  (wire-only, no PrimitiveBlock) -> for each referenced id,
  AtomicU64::store(Relaxed) at byte offset rank_if_set(id) << 3.

`SparseArrayIndex::get(id)` becomes `rank_if_set(id)` plus an
`AtomicU64::load(Relaxed)` at the same offset. Same shape as
`DenseMmapIndex`, just with the rank step in front.

What this changes:
  - Disk shrinks 2.4-2.8x (chunk + sentinel padding overhead is
    gone). Japan: 5.6 GB -> 2.0 GB. Europe extrapolation:
    52 GB -> ~29 GB (referenced_count * 8 = 3.6 G * 8).
  - Pass 1 becomes parallel: 21.1 avg cores vs 6.5 (4.2x). The
    serial chunk-streaming consumer is gone; workers `pwrite`
    via the mmap with no merge step. Reorder buffer no longer
    needed.
  - Strictly-increasing-id precondition is gone. Random arrival
    order works because each rank slot is unique and atomic.
    The CLI help text ("works on any PBF") now matches the
    implementation behavior - reviewer doc-bug catch resolved.

What this costs:
  - SparseArrayIndex carries the IdSet (with rank index) into
    pass 2. ~440 MB + ~100 MB at planet, vs the chunk format's
    ~440 MB `offsets`+`start_pad`. Net RAM is roughly flat.

First attempt used `pwrite` per tuple (299 M syscalls at japan)
and ran 10x slower (143 s pass 1). Switching to mmap + AtomicU64
matches dense's pattern and recovers the parallel win.

Result (japan sparse, plantasjen 2026-04-30, dirty bench):

| Metric | post descriptor-first | post rank-indexed flat |
|--------|-----------------------|------------------------|
| Pass 1 wall | 3.45 s | 0.82 s |
| Pass 1 avg cores | 6.5 | 21.1 |
| Pass 1 disk write | 5.59 GB | 2.01 GB |
| Total japan sparse wall | 14.3 s | 11.9 s |

Cross-validation passed (`brokkr verify
add-locations-to-ways --dataset denmark`): dense / sparse /
external all produce byte-identical output.

Europe survival measurement is the actual reason this work
exists (sparse pass-2 OOMed at europe with 1.73 TB read against
the 52 GB chunk format). See europe results below.

### Descriptor-first pass 2 pipeline - landed `e63d0b6`

`passthrough.rs::write_output_passthrough` rewritten end-to-end as a
descriptor-first parallel pipeline mirroring `external/stage4.rs`:

  HeaderWalker -> Vec<BlobDescriptor> -> partition into decode +
  passthrough -> dispatcher thread feeds decode descriptors via a
  bounded channel (16-deep) -> N decode worker threads pread +
  decompress + reframe (way) or PrimitiveBlock + BlockBuilder
  (non-way) and send (seq, result) on a 32-deep result channel ->
  consumer pre-seeds passthrough items in a `ReorderBuffer` at
  their global seq positions, drains contiguous ready items as
  decoded results arrive, calls `write_raw_owned` for passthrough
  / `write_primitive_block_owned` for decoded.

Replaces the old read-batch-rayon-drain stop-and-wait loop:
read N blobs into batch -> par_iter decode -> drain to writer ->
read next N. Read + decode + write never overlapped. The new
shape lets dispatcher reads, worker decodes, and writer-pool
compresses + writes all run concurrently; raw-frame retention
drops from a ~128-blob batch to channel depth (~32 in flight)
plus per-worker buffers.

Removed the userspace passthrough coalescing buffer
(`flush_passthrough_buf`, `coalesce_passthrough`) and the
`CopyRange` helper from this path - the consumer now hands each
passthrough frame directly to the writer's pipelined raw path
(equivalent to stage 4's choice). `BATCH_BYTE_BUDGET`,
`BATCH_MIN_BLOBS`, `BATCH_MAX_BLOBS` constants in
`commands/mod.rs` had only this caller and were dropped.

Result (japan sparse, plantasjen 2026-04-30, dirty bench best of 3):

| Metric | post `to_path_parallel` | post descriptor-first |
|--------|-------------------------|-----------------------|
| Pass 2 wall | 7.5 s | 7.5 s |
| Pass 2 peak threads | 42 | 65 |
| Pass 2 voluntary cs | 5,486 | 13,583 |
| Pass 2 peak anon | 1.43 GB | 1.64 GB |
| Disk write | 2547 MB | 2547 MB |
| Total japan sparse wall | 14.5 s | 14.7 s |

Wall is unchanged at japan because we were already CPU-bound on
pass 2 (avg cores ~20 of 22 available); the new shape adds threads
and channel queueing but cannot reduce CPU work, only overlap it.
The wins are reserved for planet scale where read + decode + write
overlap actually matters and where the writer pool (now able to
fill) is the new ceiling.

The reviewers' note about `copy_file_range` for contiguous
passthrough runs was deliberately not pursued: stage 4 (also
planet-recommended) lives without it; if measurement shows it is
the next pass 2 ceiling we can add the `write_raw_copy` opt-in
later.

Cross-validation passed (`brokkr verify
add-locations-to-ways --dataset denmark`): dense / sparse /
external all produce byte-identical output.

### Switch ALTW pass 2 writer to `to_path_parallel` - landed `7169216`

`writer_from_header_bytes_parallel` and `writer_from_header_parallel`
generalize the existing `writer_for_apply_changes` shape (renamed to
the new generic name, apply-changes' single caller updated). ALTW
pass 2 (both `write_output_passthrough` and `write_output_decode_all`)
now uses the parallel writer.

At japan scale this is invisible on wall - the ~500 MB output is far
below the ~1.5 GB/s NVMe single-thread write ceiling. Confirmed
mechanically: pass 2 peak threads went from 26 to 42 (writer pool
attached). Wall: 7.5 s -> 7.5 s. The win lands at planet scale where
the output ceilings are ~50 GB and the serial writer is the floor.

### Pass 2 wire-format way reframe - landed `cb31654`

Lifted the wire-format reframe shape from `external/stage4.rs` into
the dense / sparse pass 2 way arm. New file
`src/commands/altw/reframe.rs` exposes
`reframe_way_blob_with_locations` to `passthrough.rs::process_slot_batch`.

Way slots now take the wire-format path:

  decompress -> walk PrimitiveBlock wire format -> for each way:
    parse only id + refs, copy other fields raw, strip existing
    fields 9 / 10, append fresh fields 9 / 10 from NodeIndex::get
    lookups (zigzag-delta-encoded inline) -> compress -> write.

No `BlockBuilder`, no `StringTable::add`, no Info decode / encode,
no ref redelta, no tag re-intern. Reviewer 3's split shape:
`parse_block_top` / `process_group` / `splice_way_locations`.

Node and Unknown slots stay on the existing
PrimitiveBlock + `BlockBuilder` path; the wire-format equivalent
for nodes (untagged-node skip + partial wire edit) is a separate
follow-up item from the reviewers, not in this commit.

Result (japan sparse, plantasjen 2026-04-30, dirty bench best of 3):

| Metric | post wire-only (`044f642a`) | post reframe |
|--------|-----------------------------|--------------|
| Pass 2 wall | ~7.9 s | 7.5 s |
| Pass 2 disk write | 2553 MB | 2547 MB |
| Total japan sparse wall | 15.1 s | 14.9 s |

Modest. Pass 2 at zlib:6 is writer-bound at this scale (single
write thread + zlib:6 compression CPU per blob); reframe frees
decoder CPU which the writer queue absorbs. The Measurement Notes
section already flagged this - any pass 2 item benchmarked under
zlib:6 risks showing as "wall unchanged" while the underlying
work is genuinely cheaper. The follow-up items (descriptor-first
pipeline, `to_path_parallel`, untagged-node skip) compound: once
the writer is parallelized and node-blob CPU drops, the reframe
savings become visible.

Cross-validation passed: `brokkr verify add-locations-to-ways
--dataset denmark` shows dense / sparse / external all produce
byte-identical output.

### Pass 0 wire-only scan - landed `87f53eb`

`collect_way_referenced_node_ids` now uses
`parallel_scan_blobs_raw` (new helper in `scan/classify.rs`) +
`scan_way_refs` from `scan/way.rs`. Workers walk the wire format
directly and never construct a `PrimitiveBlock`: no StringTable
parse, no `(u32, u32)` group_ranges scratch.

Result (japan sparse, plantasjen 2026-04-30, dirty-bench best of 3):

| Metric | post rel-member (`a8db8837`) | post wire-only |
|--------|------------------------------|----------------|
| Pass 0 parallel decode wall | 1.74 s | 1.78 s |
| Pass 0 parallel decode avg cores | 5.3 | 4.5 |
| Total japan sparse wall | 14.9 s | 15.2 s |

Wall delta is within run-to-run variance at japan; the cores delta
is the real signal - per-blob CPU work dropped enough that workers
now idle waiting for descriptors. The reviewers (3 of 4) flagged
this as a planet-scale win on the way-blob classify side, where
the absolute CPU saved per blob compounds across ~50k way blobs.

The new `parallel_scan_blobs_raw` helper is symmetric with
`parallel_classify_phase` but exposes `&[u8]` decompressed bytes
to the closure. Anticipates further wire-only callers (e.g. the
relation-member wire-only scan that reviewers 3/4 flagged as
orthogonal to the per-worker-IdSet migration above).

### Migrate rel-member scan to `parallel_classify_phase` - landed `66cfa4a`

`collect_relation_member_node_ids` now mirrors the
tags-filter way-deps shape (`17b116c`): per-blob worker emits
`Vec<i64>` of member node IDs through the bounded 32-slot result
channel, main thread unions into a single shared `IdSet`. Bounds
memory to one IdSet plus per-blob transient vectors, not
N-workers x per-worker IdSet. Set-union is commutative so the
migration is correctness-preserving by construction.

Result (japan sparse, plantasjen 2026-04-30, best of 3 UUID `a8db8837`):

| Metric | `8e0cef9` (pre) | post |
|--------|-----------------|------|
| Rel-member scan wall | 4.2 s | 0.76 s |
| Rel-member scan peak anon | 4.3 GB | 0.82 GB |
| Total japan sparse wall | 20.9 s | 14.9 s |

Linear extrapolation at planet (was ~72 GB peak anon in 24 workers
x ~3 GB): now bounded by one shared IdSet (~1.3 GB at planet) plus
the 32-slot Vec<i64> queue (~640 KB / slot at planet density,
bounded ~20 MB total).

## Remaining work

### 1. Sparse encoding redesign (optional, large investment)

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

### 2. Per-batch parallel resolve (optional, lower priority)

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

## External review (2026-04-29)

Four outside reviewers were commissioned with split briefs:

- **Reviewers 1 and 2** were asked to make dense / sparse
  planet-scale safe.
- **Reviewers 3 and 4** were asked to optimize dense / sparse for
  small / medium only, treating europe+ as out of scope (external
  owns the planet path).

Reviewers 3 and 4 were operating in the doc's current frame
(dense / sparse ceiling at ~25 GB working set is structural, external
is the planet path). The planet-safety architectural rewrites from 1
and 2 are recorded at the end as a record of the option, not as
recommended direction. Findings shared across briefs (most pass 1 /
pass 2 structural items) are independent of the framing question.

### Pass 0

- ~~Replace the current `parallel_classify_phase` body, which builds
  full `PrimitiveBlock`s per blob just to iterate
  `block.elements_skip_metadata()` at `mod.rs:383`, with a wire-only
  scan via `scan_way_refs` (`src/scan/way.rs:78`). Drops per-blob
  StringTable parse and `(u32, u32)` scratch allocations entirely.
  (Reviewers 1, 3, 4.)~~ **Landed (this commit) - see "Landed work"
  above. Japan: cores 5.3 -> 4.5, wall unchanged within variance.
  Wins should compound at planet scale.**
- Run pass 0 and the relation-member scan concurrently under
  `std::thread::scope`. They read disjoint blob types, both produce
  IdSets used by pass 2, and external already runs them overlapped.
  (Reviewer 2.)

### Pass 1, dense

- The outer loop in `build_node_index_dense`
  (`src/commands/altw/dense.rs:191`) is single-threaded for
  decompress and `extract_node_tuples`; only the trivial mmap-store
  inner loop is parallelized. Replace with parallel
  pread+decompress+`extract_node_tuples` workers writing directly to
  `SharedDenseWriter` (atomic stores already make N-thread writes
  safe). The historical reason for the serial loop was 25 GB of
  cross-thread `PrimitiveBlock` heap retention; using
  `extract_node_tuples` on raw decompressed bytes inside the worker
  bypasses `PrimitiveBlock` entirely and avoids that retention.
  `NodeTuple` is 16 bytes, allocated and consumed on the same
  worker, so cross-thread retention is bounded. Reviewer 2 estimates
  ~50 lines. (Reviewers 1, 2, 3, 4.)
- Optionally retire the 128 GB virtual mmap in favor of a
  rank-compacted index. After pass 0, call
  `IdSet::build_rank_index` and allocate `referenced_count * 8`
  bytes; pass 1 writes via `rank_if_set(node_id)`; pass 2 reads via
  `rank_if_set(ref_id)`. Pattern already used by geocode pass 2
  (`src/geocode_index/builder/pass2.rs:362`, `src/idset.rs:477`).
  Cuts page faults / cache misses for sparse-id extracts; adds rank
  CPU per ref. Direct dense may still win on cache-hot dense inputs,
  so this is an intrusive benchmark, not a gated probe. Duplicate
  IDs or unsorted/corrupt inputs need atomic writes or a defined
  overwrite rule. (Reviewer 4.)

### Pass 1, sparse

- The serial consumer (single thread owning the `BufWriter`, chunk
  state machine, and byte cursor) is the structural bottleneck;
  parallel decompress workers stall waiting on it. Reviewer 3 also
  notes the chunk format is load-bearing only in service of that
  consumer; once the consumer is gone, the chunk structure stops
  earning its keep. Two replacement shapes proposed:
  - **K shard files.** Workers write per-shard files directly; final
    coalescer concatenates in node-id order. (Reviewers 2, 3.)
  - **Rank-indexed flat layout.** Pre-allocate
    `referenced.len() * 8` bytes; workers `pwrite` at
    `IdSet::rank(node_id) << 3`. Retires the chunk / start_pad
    scheme entirely and removes the strictly-increasing-id
    precondition. `SparseArrayIndex::get` becomes a bare mmap read.
    (Reviewer 3.)
- Replace the worker body with `extract_node_tuples`
  (`src/scan/node.rs:49`) instead of `PrimitiveBlock` construction,
  same reasoning as the pass 0 wire-only switch.
  (Reviewers 1, 3, 4.)

### Relation-member scan

- Replace `parallel_classify_accumulate` at `mod.rs:426` with a
  wire-only scanner walking `PrimitiveGroup` field 7 (Relation) and
  the packed `memids` field directly. The current path builds a
  full `PrimitiveBlock` per blob to read one packed varint field.
  (Reviewers 3, 4.)
- Reuse external's relation-only pread scan
  (`src/commands/altw/external/relation_scan.rs:22`) once dense /
  sparse has a shared blob plan. (Reviewer 4.)
- (Orthogonal to the per-worker-IdSet -> shared-IdSet migration,
  landed (this commit) - see "Landed work" above.)

### Pass 2 way path

- ~~Lift `reframe_way_blob_with_locations`
  (`src/commands/altw/external/stage4.rs:993`) into the dense /
  sparse pass 2 way arm at `src/commands/altw/mod.rs:630`. Copies
  the original StringTable byte-for-byte
  (`encode_bytes_field(output, 1, stringtable_bytes)`), copies
  non-way `PrimitiveGroup` fields verbatim, and for each way
  appends fields 9 / 10 to the original way bytes. No
  `BlockBuilder`, no `StringTable::add`, no Info decode / encode,
  no ref redelta, no tag re-intern. The hot path becomes:
  decompress, raw protobuf scan, coord lookup, append packed lat /
  lon, compress. (Reviewers 1, 3, 4.)~~ **Landed (this commit) -
  see "Landed work" above. Japan: pass 2 7.9 -> 7.5 s, total wall
  flat at zlib:6 (writer-bound).**
- On the reframe path, walk refs as an iterator instead of
  materializing `refs_buf: Vec<i64>` and
  `locations_buf: Vec<(i32, i32)>` at
  `src/commands/altw/mod.rs:632`; stream zigzag-delta lat / lon
  bytes directly into `packed_lats` / `packed_lons` while running
  cum-id over `refs_data`. Saves ~50-100 M small heap touches at
  europe scale. (Reviewer 3.)
- Inputs that already declare `LocationsOnWays` need existing
  fields 9 / 10 stripped before append, not appended after. Two
  extra wire-tag matches in the way walker. (Reviewers 3, 4.)
- Risks: clippy `cognitive_complexity` will fight a single-function
  implementation; reviewer 3 suggests splitting into
  `parse_block_top` / `walk_way_in_blob` / `splice_way_locations`,
  same shape as stage 4. Reviewer 4 notes compression is a
  candidate next bottleneck after the way-decode work disappears.

### Pass 2 dispatch and writer

- ~~Replace the read-batch-rayon-drain stop-and-wait loop at
  `src/commands/altw/passthrough.rs:280` with a descriptor-first
  parallel pipeline mirroring `external/stage4.rs:230+`:
  `HeaderWalker` builds the descriptor schedule (cheap, no body
  reads), partition into decode-eligible vs passthrough-eligible,
  fixed-size worker pool runs pread+decompress+reframe+assemble per
  descriptor, bounded ordered channel feeds a single consumer
  thread that only writes (and on Linux uses `copy_file_range` for
  contiguous passthrough runs). Decode, reframe, and write all
  overlap; raw-frame retention drops from a ~128-blob batch to
  channel depth + per-worker buffers. The current
  `flush before passthrough` invariant
  (`passthrough.rs:301`) becomes "drain workers in order before
  the consumer ever switches modes." (Reviewers 1, 2, 3.)~~
  **Landed (this commit) - see "Landed work" above. Japan: pass 2
  wall flat (CPU-bound saturation already). Wins reserved for
  planet. `copy_file_range` deferred - stage 4 lives without it.**
- ~~ALTW pass 2 currently routes through `to_path` (single-threaded
  write thread) via `writer_from_header_bytes`
  (`src/commands/mod.rs:352`); apply-changes already defaults to
  `to_path_parallel` (`src/commands/mod.rs:386`). Lifts the ~1.5
  GB/s NVMe write ceiling. (Reviewers 2, 4.)~~ **Landed (this
  commit) - see "Landed work" above. Japan: invisible (well below
  the write ceiling). Win is reserved for planet scale.**
- Skip output node blobs in the default
  `keep_untagged_nodes=false` mode when the blob has zero tagged
  nodes (cheap pre-scan of `dense_keys_vals` for any non-zero
  entry) AND no overlap with `relation_member_node_ids` via
  `IdSet::any_in_range` against the blob's id range. Stage 4
  already does this. Otherwise, do a partial wire-format edit that
  drops dropped nodes from `id` / `lat` / `lon` / `keys_vals`
  packed fields without rebuilding the StringTable, rather than
  full decode+re-encode. Most blobs are ~95-99 % untagged so the
  skip path is the common case. (Reviewers 1, 2, 3, 4.)
- Drop the `Vec<OwnedBlock>` per-worker buffer in `process_block`
  and `drain_batch_results`; push owned blocks directly into the
  writer's input channel (the writer pipeline already reorders by
  seq). (Reviewer 2.)

### Cross-cutting structural

- Build one `BlobMeta` table up front, mirror
  `src/commands/altw/external/blob_meta.rs:31`, drive pass 0 / pass
  1 / relation scan / pass 2 from the same plan. Removes repeated
  header walks and gives pass 2 exact frame offsets for worker pread
  / raw passthrough. (Reviewer 4.)

### Doc bug catches

- The CLI text for sparse advertises that it works on any PBF, but
  `build_node_index_sparse` requires strictly increasing node IDs.
  Either fix the help text or land the rank-indexed flat layout
  (which removes the constraint). (Reviewers 1, 3.)

### Anti-recommendations

- Do not tune `BATCH_MAX_BLOBS` / `BATCH_BYTE_BUDGET` / channel
  widths / decompression-pool sizes / sparse chunk size before the
  structural fixes land. Tuning knobs in a structurally
  bottlenecked pipeline will not move the needle. (Reviewers 2, 3.)
- Do not chase `NodeIndex::get` micro-optimization (SoA, prefetch).
  Once way blobs go through reframe, `get` is one mmap or array
  load per ref and is no longer a top item. (Reviewer 3.)
- Do not optimize sparse pass 2 further. Sparse's structural gap is
  in pass 1's serial consumer, not in pass 2. (Reviewer 3.)
- Do not try `madvise(WILLNEED)` over sorted ref ranges. The kernel
  page cache will not cooperate when the working set exceeds RAM
  regardless of advise hints; the fix has to change the access
  pattern, not the advisories. (Reviewer 2.)
- Do not add a `--index-type ramcheck` mode that picks dense /
  sparse based on free RAM. Config band-aid over a structural
  problem; another knob to debug. (Reviewer 2.)
- Land replacements as full replacements; benchmark and decide
  keep / revert. No env-var gates, no side-by-side variants.
  (Reviewer 3.)

### Planet-safety architectural rewrites

These appear only in reviewers 1 and 2 because their brief was
"make dense / sparse planet-safe." The doc's current frame is the
opposite: external is the planet path, dense / sparse stay small /
medium. Recorded so the option is not lost.

#### Reviewer 1: bounded slot / join replacement

**Diagnostic frame.** The current architecture is "build global
coordinate state, then fully decode and rebuild way blobs." Dense
allocates a fixed 16 B-entry, 128 GB virtual mmap and writes
coordinates by node id (`src/commands/altw/dense.rs:19`); even if
only referenced pages are dirtied, the working set competes with
input page cache, output writer buffers, and compression scratch.
Sparse avoids the 128 GB virtual reservation but still builds a
global chunk index plus mmap-backed values file
(`src/commands/altw/sparse.rs:28`). Both require global state
proportional to the referenced-node universe; that is the root
safety problem at planet scale.

**Replacement shape.** Slot / join based: scan way refs into
bounded buckets, resolve node coordinates by id bucket against
node blobs, emit per-way-blob coordinate payloads, stream
assembly. The source already proves this architecture internally:
stage 1 emits ref records and node-blob mapping
(`src/commands/altw/external/stage1.rs:1`), stage 2 resolves by
bucket without a global mmap
(`src/commands/altw/external/stage2.rs:1`). The rewrite is to fold
dense / sparse into that pipeline rather than maintain them as
separate global-index modes.

**Effect.** Effectively retires dense / sparse as planet modes.
Dense can remain as a small / medium fast path; sparse's identity
disappears (it is a constraint workaround, not a separate
architecture). Reviewer 1's framing: "preserving dense / sparse
identity is not worth much pre-1.0 if the architecture is wrong."

**Risk.** Large rewrite; overlaps conceptually with external.

#### Reviewer 2: streaming batch resolve

**Diagnostic frame.** The dense / sparse naming oversells the
difference. After the pass-0 referenced-node filter, both paths
physically hold ~16 GB of coord data (one 8-byte slot per
referenced node, ~2 B referenced nodes at planet); dense reserves
128 GB virtual but only ~16 GB pages get dirtied, sparse uses
~16 GB of file-backed mmap directly. Their physical hot working
set is nearly identical. The dominant pathology is pass-2 random
reads against that 16 GB store: way refs are nearly uniform across
the planet's node-id range, each blob's lookups touch the whole
coord file in arbitrary order, and on a 28 GB host the kernel page
cache holds ~10-20 GB after subtracting input readahead, output
buffers, and rayon scratch. Cross that threshold and the OS starts
evicting pages that will be touched again immediately.

**Phase 1, parallel.** Read node blobs in parallel; filter by the
pass-0 IdSet; bucket each kept `(id, lat, lon)` triple into one of
K shards by the high bits of `id` (K = 256 or 1024). Each shard
appended to its own on-disk file in input order. On a sorted PBF
that input order is also id-ascending, so each shard file ends up
sorted ascending by node id with no merge step. Concurrency: many
decode workers, each appending to its current shard; transitions
between shards are cheap because the bucket is just a high-bit
extract.

- Disk: ~16 GB total at planet (same physical size as today's
  stores, just K small files).
- RAM: bounded - per-worker output buffers, no IdSet larger than
  today's.

**Phase 2, batched merge-join.** Process way blobs in batches of N
(N ~= 64 blobs, ~512 K ways). Per batch:

  a. Decompress all N blobs in parallel; collect
     `(blob_idx, ref_position, node_id)` triples (~5 M triples per
     batch at planet, easy memory).
  b. Bucket triples by shard; sort each bucket by `node_id`.
  c. For each shard, sequentially scan the shard's coord file
     until every requested id has been resolved. Single forward
     pass, kernel readahead carries the load. Multiple shards
     scan in parallel.
  d. Scatter resolved coords back to per-blob, per-way arrays.
  e. Re-encode each way blob in parallel and emit through the
     existing writer pipeline.

**Why this is the rewrite, not just another mode.** Pass 2's RAM
bound becomes O(batch), not O(referenced nodes). Coord shards are
touched sequentially, so the page cache only needs the small
forward window per shard, not the full 16 GB; the pass survives
with as little as ~1 GB free RAM. All decompress is parallel, all
re-encode is parallel, writes go through the existing parallel
writer, the coord shards are private temp files so no O_DIRECT
alignment fights. Dense's strength ("lookup is one mmap load") and
sparse's strength ("no 128 GB virtual reservation") collapse into
the same shape, and that shape is also planet-safe at 28 GB.

**Comparison to external.** ~16 GB total temp vs external's ~224
GB, because the coord shards do not materialize per-ref records.
Reviewer 2 claims this could deprecate dense, sparse, and external
in one move (except for adversarial unsorted input).

**Risk.**
- Implementation surface is roughly stage 1 + stage 2 size. The
  external codebase already provides every supporting primitive
  (sharding, parallel scan, scratch dirs, `BlobMeta`,
  `ReorderBuffer`, parallel writer); the work is remixing, not
  inventing.
- Tuning K (shard count) and N (batch size) matters. Wrong K
  produces either too many open files or too-large per-shard
  windows; wrong N produces either too little parallelism or too
  much per-batch RAM.
- Output ordering: way blobs must remain in input order. The
  existing reorder / writer pipeline already handles this; the
  passthrough flush invariant just becomes "drain workers in order
  before the consumer switches modes."

## Measurement notes

Pass 2 CPU wins (wire-format reframe, descriptor-first pipeline,
`to_path_parallel`, untagged-node skip) can be invisible on wall
time under default `zlib:6` because decoder CPU freed by these
items refills the writer queue. Measure any pass-2 item under both
`zlib:6` and a non-default such as `zstd:1` or `compression:none`
to confirm the win is real and not a decoder-shifted writer-bound
case.

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
