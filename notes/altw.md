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
| Sparse | safe | safe | safe (5:59, ~25-30% slower than external) | thrash (29-bytes-per-node × ~2 G referenced > cache) |
| External | safe | safe | safe | safe |

`dense` was removed: sparse rank-indexed flat is faster than the
prior dense path at every measured scale (japan dense 51.6 s vs
sparse 11.9 s, 4.3x), and works in regimes dense did not (europe
survives at ~6 minutes on a 27 GB-RAM host). See "Don't re-attempt"
below for the reasoning, and the dense-removal commit for the
breaking-change notes.

After the `c6f08ff` rank-indexed flat layout, sparse europe goes
from "OOM at 9:56" to "completes in 5:59" - the chunk format's
~52 GB working set became ~29 GB, fitting close enough to the
host's ~25 GB free cache margin to bound (not eliminate) the
fault rate. Sparse is now competitive with external at europe.
Sparse planet has not been tried (likely thrashes at ~60 GB
working set); external remains the planet-recommended path.

## Measured walls (plantasjen)

Two distinct optimization arcs landed against this code:

**Arc 1 (2026-04-29, commits `68806b0` -> `8e0cef9`):** parallelize
pass 1, inline NodeIndex::get in pass 2. Sparse went from "1.5x
slower than dense at japan" to "2.5x faster than dense at japan."

| Dataset | Mode | `68806b0` (pre) | `29683ee` (parallel pass 1) | `8e0cef9` (inline pass 2) |
|---------|------|-----------------|------------------------------|----------------------------|
| Denmark | dense | 11.9 s | - | - |
| Denmark | sparse | 17.3 s | 15.6 s | **5.8 s** |
| Japan | dense | 51.6 s | - | - |
| Japan | sparse | 78.4 s | 71.7 s | **20.9 s** |

**Arc 2 (2026-04-30, commits `66cfa4a` -> `c6f08ff`):** five-item
reviewer plan + sparse rank-indexed flat layout. The reviewer plan
freed CPU; the rank-indexed flat layout shrunk the sparse working
set 2.4-2.8x and made europe sparse survive.

| Dataset | Mode | `8e0cef9` baseline | `e63d0b6` (5-item) | `c6f08ff` (rank flat) |
|---------|------|-------------------|---------------------|------------------------|
| Japan | sparse | 20.9 s | 14.3 s | 11.9 s |
| Europe | sparse | OOM at 9:56 | not measured (still chunk format) | **5:59** |

Per-phase profile at the final state (japan sparse, commit
`c6f08ff`, UUID `aa4fe496` hotpath / `158a86d7` bench):

| Phase | Wall | Avg cores | Note |
|-------|------|-----------|------|
| Pass 0 | 2.5 s | ~5 | wire-only scan |
| Pass 1 | 0.8 s | 21.1 | rank-indexed parallel mmap-write |
| Rel-member scan | 0.8 s | 1.0 | shared IdSet via parallel_classify_phase |
| Pass 2 | 7.3 s | 20.5 | compression-bound at zlib:6 |

Pass 2 is now the headline floor at ~7.3 s on japan. Hotpath shows
`frame_blob_into` (compress + frame) is 1027% of wall (~10 cores
worth) - see Findings below.

Counters at scale (`altw_referenced_node_ids` x 8 bytes is the
sparse working set after `c6f08ff`):

| Counter | Denmark | Japan | Europe |
|---------|---------|-------|--------|
| `altw_referenced_node_ids` | 49 M | 299 M | 3,617 M |
| `altw_relation_member_node_ids` | 25 K | 193 K | 10.6 M |
| Sparse temp file (chunk format, pre `c6f08ff`) | 1.0 GB | 5.7 GB | ~52 GB |
| Sparse temp file (rank-indexed flat) | 0.4 GB | 2.0 GB | ~29 GB |

## Findings

### Dense fails above ~25 GB working set (historical; dense removed)

Recorded for context - dense (`--index-type dense`) was removed
after the rank-indexed flat sparse layout dominated it at every
measured scale. The original failure-mode characterization that
motivated the removal:

Touched mmap pages scale linearly with `altw_referenced_node_ids`
x 8 bytes:

- Denmark: 49 M x 8 = 393 MB. Fits trivially.
- Japan: 299 M x 8 = 2.4 GB. Fits, 3.1 M pass-1 majflt indicates
  the page cache was already churning a bit.
- Europe: 3,617 M x 8 = 29 GB. Exceeds the 27 GB-free host.
  Catastrophic page-thrash: 12 M majflt in pass 1 (4m18s), 23 M
  majflt in 13 minutes of pass 2 before SIGKILL. 2.1 TB read off
  disk for 35 GB of input.

Architectural, not a tuning gap. Pre-pass-0 filtering already
restricts to way-referenced nodes; the working set was those nodes
times 8 bytes. Above host free RAM, dense could not work.

Sparse rank-indexed flat has the same 8-bytes-per-node working set
as dense, so it has the same upper limit at planet (~60 GB) - the
encoding doesn't shrink the working set, it shrinks the chunk
format's overhead. The reason sparse rank-flat *survives* europe
where dense did not is that pass 1 is parallel mmap-write (no
serial consumer) and pass 2 reads via `rank_if_set + mmap` (no
chunk indirection): the access pattern is cleaner, fault behavior
is more predictable, and the working set fits the host's free
cache margin closely enough to bound the fault rate. At planet
scale neither encoding will fit; external remains the planet path.

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

Migrated to `parallel_classify_phase` in commit `66cfa4a`;
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

### Pass 2 floor at zlib:6 is compression CPU (hotpath UUID `aa4fe496`)

Hotpath profile of japan sparse at commit `c6f08ff` (post 5-item
arc + rank-indexed flat). Pass 2 wall 7.45 s; total CPU split:

| Function | Total CPU across threads | % of wall |
|----------|--------------------------|-----------|
| `write::framing::frame_blob_into` (compress + frame) | 123.1 s | 1027% (~10 cores) |
| `process_block` (Node decode + BlockBuilder) | 21.1 s | 176% |
| `decompress_blob_raw` | 18.2 s | 152% |
| `write_primitive_block_owned` (dispatch) | 5.3 s | 44% |
| `add_node` | 1.7 s | 14% |

Compression is ~75% of pass-2 CPU; pass 2 is fully core-saturated
(avg cores ~21 of 22). Decode work (process_block + decompress
+ add_node = ~41 s = ~3.4 cores) is genuinely a small fraction.

Compression sweep at japan sparse (single bench each):

| Compression | Wall | Δ vs zlib:6 |
|-------------|------|-------------|
| zlib:6 (default) | 11.9 s | - |
| none | 9.7 s | -18% |
| zstd:1 | 8.9 s | -25% |

Implication: pass-2 CPU optimizations (untagged-node skip, partial
wire-format edits, etc.) cannot move wall under default zlib:6
because freed decoder CPU just refills the writer queue. Same
diagnostic that closed the stage-4 wire-format DenseNodes filter
in `notes/altw-optimization-history.md` (the "writer ceiling
diagnostic" lesson). Any future pass-2 item must measure under
both `zlib:6` and `zstd:1` (or `compression:none`) to confirm the
win is real.

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

### Sparse rank-indexed flat layout - landed `c6f08ff`

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

**Europe sparse survives** (UUID `f9a61784`, plantasjen
2026-04-30, 35.3 GB input, 28 GB host RAM, ~25 GB free cache):

| Phase | Wall | Notes |
|-------|------|-------|
| Pass 0 | 63.2 s | wire-only scan, 22.5 GB header + body reads |
| Pass 1 | 57.7 s | 28 GB sparse temp file written, avg cores 11.6 |
| Rel-member scan | 1.24 s | bounded as before |
| Pass 2 | 197.0 s | 6.8 M majflt, 251 GB read, avg cores 13.9 |
| Total | **5 min 59 s** | exit 0, output validated |

Compared to yesterday (chunk-format europe sparse OOM'd at 9 min
56 s, 19.7 M majflt, 1.73 TB read) the rank-indexed flat layout:
  - Survives (33 GB working set fits within margin of 25 GB
    cache; we still page-fault but bounded).
  - 65% fewer pass-2 majflts (6.8 M vs 19.7 M).
  - 7x less pass-2 disk read (251 GB vs 1.73 TB).

Compared to external at europe (which is the planet-recommended
path):
  - External default zlib:6: 4 min 31 s - 5 min 22 s.
  - External zstd:3: 3 min 53 s.
  - Sparse rank-indexed flat: 5 min 59 s (~25-30% slower).

Sparse at europe is now within striking distance of external.
The 29 GB working set is still slightly cache-oversubscribed; if
we reduce the encoding further (delta-encoded packed lat/lon, or
i64 -> i32 quantization beyond decimicrodegrees) sparse could
close the gap or even win. As is, sparse is a viable alternative
at europe scale rather than a non-starter.

Planet sparse hasn't been tried yet. The working set scales with
referenced_node_ids; at planet that is roughly 2x europe (or
more), so ~60-70 GB - well above the host's free cache. Without
a smaller encoding sparse will likely thrash again at planet.
External remains the planet-recommended path for now.

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
for nodes (untagged-node skip + partial wire edit) was tried as
a follow-up in the same session and reverted - see "Don't
re-attempt" for why.

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

### 1. Further sparse encoding shrink (planet-only, speculative)

**Status:** speculative. Land only if there is a real workload need
for sparse at planet scale that external doesn't already serve.

**Where we are after `c6f08ff`:** rank-indexed flat layout shrunk
the sparse temp file 2.4-2.8x. Japan 5.7 GB -> 2.0 GB; europe
~52 GB -> ~29 GB; planet projection ~60 GB (still above the
~25 GB cache budget on plantasjen-class hardware). Europe sparse
now survives at 5:59; planet sparse not yet measured but expected
to thrash at ~60 GB working set.

**Path forward (unmeasured, optimistic estimates):**

- Drop precision: i16 lat/lon would halve the encoding to 4
  bytes/node. Loses sub-microdegree precision (~1 cm at equator).
  Lossy - probably not viable without explicit project sign-off.
- Per-blob origin/granularity reuse: store i32 deltas relative to
  a per-blob origin (already in PBF wire format). Requires per-
  ref blob lookup, costing CPU. Complexity vs benefit unclear.
- Use existing PBF `granularity` (default 100 nanodegrees) more
  aggressively - i16 + per-blob lat/lon offset would be ~4
  bytes/node lossless if every node fits within a per-blob
  bounding box. PBF blob bboxes vary; not always tight enough.

**Alternative:** keep external as the planet-recommended path,
document sparse as medium-scale-or-smaller. (Current state.)

### 2. Per-batch parallel resolve (optional, lower priority)

**Status:** ranked low. Inline lookup at small / medium scale wins
on its own merits; the regime where global-locality sort would help
(europe sparse) is also the regime where the rank-indexed flat
layout already paid for itself by shrinking the working set.
Global sort doesn't help further at europe and sparse planet still
fails for the working-set reason regardless. Not worth pursuing
without an encoding fix that gets sparse planet inside cache.

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
- **Re-introducing `--index-type dense`.** Dense was removed
  after rank-indexed flat sparse dominated it at every measured
  scale: japan dense 51.6 s vs sparse 11.9 s (4.3x), europe dense
  OOM vs sparse 5:59. The remaining reviewer items for dense
  (parallel pass 1, retire 128 GB virtual mmap in favor of
  rank-compacted index) would have *converged* dense to sparse
  rank-flat anyway - same encoding, same access pattern, same
  wall. Re-adding dense as a "simpler-to-reason-about fallback"
  is a cosmetic bet against a measurement-anchored consolidation;
  don't.
- **Untagged-node skip-entirely as a wall-time optimization at
  small/medium.** Phase 1 of the reviewer item (skip an output
  node blob if `!has_tags && !relation_member_node_ids.any_in_range`)
  was implemented and reverted in this same session: at japan,
  zero blobs qualified for skip-entirely (every node blob has at
  least one tagged node OR overlaps a relation member). Wall flat,
  no measurement signal. The reviewer's "common case" framing
  refers to the *partial wire-format edit* path (drop 95-99% of
  untagged nodes per blob, keep StringTable). The skip-entirely
  alone is a planet-scale-only win at best, and now also blocked
  by the compression-CPU floor finding (Pass 2 wall is bounded by
  `frame_blob_into` at zlib:6 - freed decoder CPU just refills the
  writer queue). Re-attempt only if measurement at planet scale
  shows a non-trivial fraction of node blobs hit the skip path,
  AND the run is under zstd:1 / `compression:none`.
- **Standalone "streaming batch resolve" / "slot+join" as a fourth
  `--index-type`.** Reviewer 2's pitch in the "External review"
  block below: replace dense + sparse + (optionally) external with
  a single streaming external join. Evaluated at session end and
  declined: the design is not categorically different from the
  existing `external` mode (also a streaming external join with
  bucketed shards). External has had 12+ months of incremental
  optimization (1462 s -> 661 s on planet, see
  `altw-optimization-history.md`); a Reviewer-2-shape rebuild starts
  many wins behind from day 1. The closest precedent
  (`altw_v2`, 2026-04-16) failed at europe specifically because it
  was in-RAM (Reviewer 2's design avoids that), so the past failure
  doesn't disqualify the design - but the burden of "must beat
  heavily-optimized external" is steep. Live opportunity work for
  external lives in `altw-external.md`. Reviewer 2's design stays
  recorded below as a "record of the option," not the recommended
  direction. Re-attempt only if a measurement-anchored thesis
  identifies a specific weakness in existing external that
  Reviewer 2's structure addresses.

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
  (Reviewers 1, 3, 4.)~~ **Landed `87f53eb` - see "Landed work"
  above. Japan: cores 5.3 -> 4.5, wall unchanged within variance.
  Wins should compound at planet scale.**
- Run pass 0 and the relation-member scan concurrently under
  `std::thread::scope`. They read disjoint blob types, both produce
  IdSets used by pass 2, and external already runs them overlapped.
  (Reviewer 2.)

### Pass 1, dense

- ~~The outer loop in `build_node_index_dense` is single-threaded
  for decompress and `extract_node_tuples`; only the trivial
  mmap-store inner loop is parallelized. Replace with parallel
  pread+decompress+`extract_node_tuples` workers... (Reviewers
  1, 2, 3, 4.)~~ **Obsolete: dense was removed (see "Status" and
  "Don't re-attempt"). The parallel-pass-1 + extract_node_tuples
  shape now exists in sparse rank-flat (`c6f08ff`).**
- ~~Optionally retire the 128 GB virtual mmap in favor of a
  rank-compacted index. After pass 0, call
  `IdSet::build_rank_index` and allocate `referenced_count * 8`
  bytes... (Reviewer 4.)~~ **Obsolete in spirit: dense was
  removed. The rank-compacted shape Reviewer 4 described *is* what
  sparse rank-flat is now (`c6f08ff`).**

### Pass 1, sparse

- ~~The serial consumer (single thread owning the `BufWriter`,
  chunk state machine, and byte cursor) is the structural
  bottleneck; parallel decompress workers stall waiting on it.
  Reviewer 3 also notes the chunk format is load-bearing only in
  service of that consumer; once the consumer is gone, the chunk
  structure stops earning its keep. Two replacement shapes proposed:
  K shard files (Reviewers 2, 3) or **Rank-indexed flat layout**
  (Reviewer 3).~~ **Rank-indexed flat layout landed `c6f08ff` - see
  "Landed work" above. Japan pass 1 wall 3.45 s -> 0.82 s (4.2x);
  europe sparse went from OOM-at-9:56 to completing in 5:59. Disk
  shrinks 2.4-2.8x.**
- ~~Replace the worker body with `extract_node_tuples`
  (`src/scan/node.rs:49`) instead of `PrimitiveBlock` construction,
  same reasoning as the pass 0 wire-only switch.~~ **Landed
  `c6f08ff` (folded into the rank-indexed flat work).**

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
  landed `66cfa4a` - see "Landed work" above.)

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
  lon, compress. (Reviewers 1, 3, 4.)~~ **Landed `cb31654` -
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
  **Landed `e63d0b6` - see "Landed work" above. Japan: pass 2
  wall flat (CPU-bound saturation already). Wins reserved for
  planet. `copy_file_range` deferred - stage 4 lives without it.**
- ~~ALTW pass 2 currently routes through `to_path` (single-threaded
  write thread) via `writer_from_header_bytes`
  (`src/commands/mod.rs:352`); apply-changes already defaults to
  `to_path_parallel` (`src/commands/mod.rs:386`). Lifts the ~1.5
  GB/s NVMe write ceiling. (Reviewers 2, 4.)~~ **Landed `7169216`
  - see "Landed work" above. Japan: invisible (well below the
  write ceiling). Win is reserved for planet scale.**
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

- ~~The CLI text for sparse advertises that it works on any PBF,
  but `build_node_index_sparse` requires strictly increasing node
  IDs. Either fix the help text or land the rank-indexed flat
  layout (which removes the constraint). (Reviewers 1, 3.)~~
  **Resolved by `c6f08ff` (rank-indexed flat layout). The
  precondition is gone; CLI text and implementation now agree.**

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
