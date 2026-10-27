# Performance Data

Current planet/europe baselines and current per-command phase breakdowns
across the active architecture. Optimization arcs, retired phase
breakdowns, and old commit-pinned cross-dataset tables live in
[performance-history.md](performance-history.md).

## Host: plantasjen

- CPU: AMD (details via `brokkr env`)
- RAM: 30 GB
- Swap: 8 GB
- Storage: nvme (source, data, scratch), hdd (target/cargo)
- Governor: performance
- Profile: `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`

## Datasets

| Dataset | Raw PBF | Indexed PBF | ALTW PBF | Elements |
|---------|---------|-------------|----------|----------|
| Malta | 8 MB | 8 MB | - | ~1M |
| Greater London | 122 MB | 122 MB | - | ~17M |
| Denmark | 461 MB | 465 MB | - | 59M |
| Switzerland | 524 MB | - | - | - |
| Norway | 1.4 GB | 1.4 GB | - | - |
| Japan | 2.4 GB | 2.4 GB | - | 344M |
| Germany | 4.5 GB | 4.5 GB | - | - |
| North America | 18.8 GB | 18.8 GB | - | 2.58B |
| Europe | 32.4 GB | 33.6 GB | - | 4.2B (3.7B nodes, 454M ways, 8.2M rels) |
| Planet | 87.3 GB | 87.7 GB | 88.4 GB | 11.6B (10.4B nodes, 1.17B ways, 14.1M rels) |

## Reading rules

How a keep/revert verdict is read off this file and `.brokkr/results.db`.
Calibrated 2026-07-10 at commit `6d6e158` on denmark
`add-locations-to-ways` across all three modes: clean bench `7bd88e83`
(4.0 s wall, 1.49 GB peak anon), hotpath `53ed877e` (3.9 s), alloc
`a7e1159b` (4.3 s, ~7 GB cumulative churn).

Instrumentation here is sparse and per-command. Where a single production
pipeline would be annotated densely and pay a large fixed profiling tax,
pbfhogg spreads a handful of coarse `hotpath::measure` points across each
command, so `--hotpath` / `--alloc` overhead is workload-dependent and
usually small.

- **Verdicts come from `--bench 3` best-of, not a single `--bench 1` run.**
  `brokkr <cmd> --bench` defaults to three runs and stores the best. Planet
  is often run `--bench 1` on cost, so a single-shot delta of a few percent
  at sub-150-s walls, and a larger fraction at sub-10-s walls, sits inside
  observed bench-to-bench variance (this file flags such rows "within
  noise"). A spec claiming a win states the expected bound up front and the
  verdict is read against that bound, not against zero.
- **`--hotpath` ranks; it does not measure absolute wall.** On the denmark
  calibration hotpath wall was 3.9 s against a 4.0 s clean bench - no
  measurable inflation, because altw's annotated functions are coarse
  (hundreds of microseconds to milliseconds per call over a few thousand
  calls). The per-call record cost only surfaces on an annotation taking
  tens of millions of sub-microsecond calls, which pbfhogg's sparse
  instrumentation mostly avoids. Use hotpath to see where the time goes; read
  the clean bench for the wall.
- **A hotpath percentage over 100% is not an error.** It is cross-thread CPU
  seconds relative to wall clock: on the denmark run `frame_blob_into`
  reports 614% and `process_block` 158%, the parallel compression and
  block-processing pools spending several core-seconds per wall-second.
- **Neither `--hotpath` nor `--alloc` emits a peak-RSS figure.** Both run
  under the instrumentation features and produce no `/proc` sidecar timeline
  at all - only a point-in-time thread snapshot (415.9 MB hotpath, 315.6 MB
  alloc on the denmark run), which is not the peak and here sits below the
  clean 1.49 GB. Peak anon RSS comes from `--bench` or plain runs only.
- **`--alloc` numbers are valid only from commit `678478d` onward.** That
  commit installs `CountingAllocator` as the global allocator under the
  hotpath-alloc feature; before it, tracking was not wired to the global
  allocator and byte totals undercount. The denmark alloc run above is post
  `678478d`, so its ~7 GB churn - led by `parse_and_inline_with_scratch`
  1.7 GB, `parallel_scan_blobs_raw` and `parallel_classify_phase` 1.6 GB
  each - is real; older alloc profiles do not compare.
- **`writer_write_ns` is retired (2026-10-26); it never meant one thing.**
  On the buffered, `O_DIRECT` and io_uring backends it was writer-thread
  wall inside the backend write; on the `pwrite` pool (the default for
  `apply-changes` and add-locations-to-ways pass 2) it was pool-thread time
  summed across every worker, up to the pool size times wall. Runs from that
  date carry `writer_handoff_ns` (the handoff thread's elapsed time per
  chunk, the same definition on every backend, the pool's dispatch
  included) and `writer_pool_write_ns` (the pool's summed execution time,
  pool backend only); the definitions are on the counters in
  `src/write/metrics.rs`. An old `writer_write_ns` compares with
  `writer_pool_write_ns` on a pool-backend command (the pool's timer did not
  move) and roughly with `writer_handoff_ns` on every other backend, whose
  timer now also covers flattening a framed chunk, dropping the payload and
  the header write. `writer_bytes_written` kept its name but
  now covers the header frame and the synchronous writer's raw writes
  (`multi-extract`), so on a successful run writing fresh regular files it
  equals the summed output size on every backend; older
  rows undercount by the header on pipelined backends and by every raw
  write on synchronous ones. The synchronous writer's `writer_payload_*`
  counters, previously always zero, count too.
- **Sidecar rows from `56a50e0d` until `c2409c45` do not compare on the
  send-side counters.** `56a50e0d` sized the ordered engine's result channel
  to the admission window, which no run can fill, so a converted pool's
  sends never blocked there and the wait moved to the uncounted admission
  gate: `s4_send_ms`, the `WAIT_S4_SEND` and `WAIT_P2_SEND` spans and
  `altw_pass2_send_ms` read near zero for it. `c2409c45` restored the channel
  depth, and the engine now records its own gate, send and receive waits
  (`ordered_pool_*_wait_ns`) for every ordered pool. A row from that range
  does not compare with either side on those counters.

## Cat passthrough (indexdata generation)

No `--type` filter. Decompresses each blob to scan IDs/tags, reframes BlobHeader
with indexdata+tagdata, preserves original compressed bytes. No re-compression.

| Dataset | Size | Time | Notes |
|---------|------|------|-------|
| Denmark | 461 MB | **2.8s** | commit `69a127f`, buffered |
| Europe | 32.4 GB | 112s | commit `69a127f`, `--direct-io`, `--type node,way,relation` (filtered path, +3.8% size) |
| Planet | 87 GB | **86.5s** | commit `aee7727`, buffered, UUID `5d90623f`, `--bench 1` |

Passthrough is buffered-only; `--direct-io` adds alignment overhead without
the concurrent read/write pattern that makes it faster for merge.

The `--clean` path measured below ran `parallel_classify_phase` once per
kind with framed output streamed via `ReorderBuffer` (commits `6184602` +
`b347c0a`, 2026-04-27). Planet `cat --clean version`: **5m34s wall, 750 MB
peak anon** (UUID `f2315551` at `4fc8e35`, 2026-04-27 overnight; previously
`7c4e03eb` 5m48s/835 MB at `b347c0a` re-bench - 14 s wall, 10 % anon
improvement attributable to single-shot variance). The path has since become
one `parallel_classify_phase_ordered` pass per input (every scheduled blob
decoded once, output in input order); that shape has not been measured yet,
so these figures describe the per-kind one.

Sidecar phases across the two shapes: the per-kind shape's `CAT_NODES_*`,
`CAT_WAYS_*` and `CAT_RELATIONS_*` pairs bracketed execution barriers. The
one-pass shape at `8da26b23` replaced them with a single `CAT_FILTERED_*`
pair; a later change (2026-10-26) brought the per-kind names back inside it,
as write-side kind transitions (`KindPhase` in `src/commands/cat/mod.rs`).
Decode of the next kind overlaps a row by up to the pool's admission window
of tasks, and the wait for the first next-kind result lands in the row
before, with no time bound the code enforces. So a per-kind row compared
against this baseline is approximate, the short relation row least of all;
the wall is the exact comparison, and `CAT_FILTERED_*` the exact ordered-pass
total (it excludes the schedule walk and the final flush). Runs from
`8da26b23` until that change carry no per-kind rows.

## Merge (apply-changes)

Best results per dataset.

| Dataset | Size | buffered+none | buffered+zlib | uring+none | uring+zlib |
|---------|------|---------------|---------------|------------|------------|
| Malta | 9 MB | 14 ms | 42 ms | - | - |
| Greater London | 124 MB | 140 ms | 333 ms | - | - |
| Denmark | 487 MB | 218 ms | 331 ms | - | - |
| Switzerland | 529 MB | 561 ms | 1.22s | - | - |
| Norway | 1.4 GB | 549 ms | 747 ms | - | - |
| Japan | 2.4 GB | 1.87s | 2.88s | - | - |
| Germany | 4.7 GB | 3.42s | 5.34s | 4.4s | 9.6s |
| North America | 18.8 GB | 14.9s | 17.3s | **11.9s** | 15.2s |
| Planet | 87 GB | 532s | 753s | - | - |

Germany (4.7 GB, 146K-change daily diff): rewrite fraction 18.4%.
North America (18.8 GB, 645K-change daily diff): 303K passthrough / 19.6K
rewritten blobs. All variants under 600 MB RSS.
Planet (87 GB, daily diff): 86% rewrite, 1.8 GB RSS.

io_uring crossover at ~4-5 GB input. Below that, page cache absorbs everything.
At NA scale (18.8 GB exceeds 30 GB page cache), O_DIRECT + async I/O delivers
12-20% improvement. sqpoll adds no measurable benefit (<1%).

### Descriptor-first streaming pipeline

Three-stage pipeline replacing the per-batch rayon barrier:

- Scanner walks blob headers via `HeaderWalker`; non-overlap indexed
  blobs route to the drain as `DrainItem::CopyRange` (never reach the
  worker pool); overlap candidates go to a long-lived worker pool. Under
  `--locations-on-ways`, when the OSC creates or modifies a node, way
  blobs are the exception: every one goes to the workers to be searched
  for stale inline coordinates (see "Inline-location refresh" below).
- Workers pread body, decompress, precise-check; under
  `--locations-on-ways` they extract node coords during the node phase
  into per-worker `Arc<Mutex<FxHashMap>>` slots. Drain merges slots at
  the node→way barrier, publishes via `LocMapHandle`, signals the
  scanner. Way-phase workers read the published map to resolve OSC way
  refs.
- Workers call `frame_blob_pipelined` inline and attach framed
  `Vec<u8>` chunks to `DrainItem::Rewritten`; drain uses
  `write_raw_owned` per chunk (avoids the writer's
  rayon-spawn-per-block dispatch).

Caveat on the planet and europe LOW figures in this section: they were
taken on the `altw` variants, which `add-locations-to-ways` made without
`--keep-untagged-nodes`. On those bases most way-node refs of the OSC's
ways point at dropped nodes, so these runs left millions of refs
unresolved (planet `2161ea62`: 4,237,222) and wrote `(0, 0)` for them,
before the missing-node guard existed. They time a merge that resolved
little, and the current code refuses that run unless
`--ignore-missing-nodes` is passed. "Inline-location refresh" below has
figures on a base that keeps its untagged nodes.

Planet LOW altw + OSC 4913, `--bench 1`, plantasjen (source on
Banan/nvme1n1; cross-disk scratch on Booty/nvme0n1p3 for the
separate-drives experiment). Parallel pwrite is the default writer
backend for `apply-changes` (buffered fallback removed from that path
on 2026-04-21). The columns below show the three backends as measured;
the same-disk `--io-uring` column was re-measured at commit `16e3694`
(2026-04-26) after the CopyRange corruption fix (commit `fa8251d`)
landed.

| Config | Buffered (removed) | `--io-uring` | Parallel pwrite (default, POOL_SIZE=16) |
|---|---:|---:|---:|
| Same-disk, `--compression none` | 135.5 s | **137.5 s** ¹ | 116.0 s |
| Same-disk, `--compression zlib:6` | 143.7 s | **137.4 s** ¹ | 140.8 s |
| Same-disk, `--compression zstd:1` | 121.2 s | **126.3 s** ¹ | 104.5 s |
| Cross-disk, `--compression none` | 95.4 s | 93.0 s ² | 99.0 s |
| Cross-disk, `--compression zlib:6` | 134.9 s | 127.9 s ² | 117.4 s |
| Cross-disk, `--compression zstd:1` | 87.1 s | 82.8 s ² | **80.9 s** |

¹ Post-`fa8251d` re-measurement at `16e3694`, 2026-04-26 (UUIDs
`9a5c25a7` / `70e5414b` / `0e6a5918`).

² Cross-disk io-uring rows still pre-`fa8251d` and not re-measured -
treat as tainted until refreshed.

Best: **80.9 s** at zstd:1 + cross-disk + parallel pwrite. Same-disk
best on the fixed writer: **104.5 s** at zstd:1 + parallel pwrite.

Writer-backend rule (parallel pwrite is the default; `--io-uring` kept
as an opt-in override for IOPS-bound cross-disk topologies):

- **Same-disk**: parallel pwrite wins at every compression level. The
  pre-fix io-uring advantage was an artefact of the CopyRange offset
  bug; on the fixed writer, parallel pwrite's 16 concurrent pwrites
  beat io-uring's queue-depth batching at every same-disk compression
  point measured.
- **Cross-disk** + zstd:1 / zlib:6: parallel pwrite wins (80.9 s at
  zstd:1, 117.4 s at zlib:6). Disk has write bandwidth headroom;
  parallel pwrite saturates it faster than a single-thread writer can.
  Compressed-output rule: cross-disk favours parallel pwrite at every
  compressed level measured.
- **Cross-disk** + `--compression none`: io-uring's pre-fix 93.0 s
  advantage over parallel pwrite's 99.0 s sits inside the same
  CopyRange bug; whether it survives re-measurement on the fixed
  writer is open.

Same axis points re-measured at europe scale on the fixed writer
(`16e3694`, 2026-04-26): same-disk io-uring at none / zlib:6 / zstd:1
= **57.9 s / 58.5 s / 53.9 s** (UUIDs `377ac699` / `0d62d01a` /
`42b24498`).

Pool-size sweep at cross-disk zstd:1 (plantasjen, Samsung 990 PRO):
4 → 89.2 s, 8 → 83.4 s, 16 → **80.9-82.1 s** (two runs), 32 → 82.2 s.
POOL_SIZE=16 is hard-coded in
[`src/write/parallel_writer.rs`](../src/write/parallel_writer.rs); the
comment explains the measurement. Over-contends above 16.

### Same-disk `-j N` sweep (LOW + zstd:1, planet, commit `16e3694`)

Default writer (parallel pwrite, POOL_SIZE=16). `-j` is the
descriptor-first scanner's worker pool size; default auto-detect on
this 24-thread host picks `nproc - 2 = 22`.

| `-j` | Wall | UUID |
|---:|---:|---|
| 4 | 173.8 s | `10f8ddf2` |
| 8 | 120.8 s | `54f7fd4e` |
| 16 | 106.2 s | `2161ea62` |
| 24 | 107.8 s | `0b3829bc` |

Saturates at `j16`; `j24` is within single-shot noise. Matches the
table-row same-disk parallel pwrite zstd:1 (104.5 s) within noise -
confirming the writer-pool ceiling at POOL_SIZE=16, not the scanner
ceiling.

### Inline-location refresh (`--locations-on-ways`, commit `81f3fbe4`)

Since `81f3fbe4` a surviving base way takes the OSC coordinate for every
ref whose node the OSC creates or modifies. Way blobs the merge used to
splice unread are now read, decompressed and searched by a wire-level scan
(clean ones are still spliced); surviving ways in rewritten blobs are
refreshed while being written. The same commit seeds
`block_overlaps_diff`'s cursors by binary search, which used to walk the
whole diff ID prefix for every candidate blob.

Europe `altw-keep-untagged` (seq 4714, every node kept) + OSC 4715,
bygg (Ryzen 9 9950X3D2, 30.5 GiB, one WD SN850X for source, output and
scratch), interleaved `--bench 1` cells, no trim between them:

| Commit | Walls (s), in run order | UUIDs |
|---|---|---|
| `8f8ef92a` (before) | 45.9 (an hour earlier), 60.8, 63.3, 56.8 | `78858442`, `6c41fa81`, `191f94a0`, `f93ebae1` |
| `81f3fbe4` (fix) | 48.9, 55.8, 62.1 | `6d0d2213`, `2dc84b53`, `32c37ec1` |

The walls do not separate: each cell writes a 56 GB output and drive
state drifts across the series, and the fix comes out faster in two of
three adjacent pairs. The worker counters do (summed elapsed over 22
workers, `6c41fa81` before, `2dc84b53` after): decompress +42.6 s (way
blobs that used to be spliced unread), refresh scan +30.8 s, rewrite
+34.5 s, frame +29.5 s (the about 5% more way blobs recompressed), and
precise check -86.2 s from the cursor seeding. Net about +55 s summed,
about 2.5 s per worker; the sidecar's way phase went from 28.1 s at 18.8
cores to 30.0 s at 22.2 cores. The refresh refreshed 74,194 refs in
23,221 ways, scanned 29,766 way blobs and rewrote 2,951 only for the
refresh.

Planet `altw-keep-untagged` (seq 4912, made with `--index-type external`,
10,447,738,627 nodes, about 17.5K way blobs) + OSC 4913, bygg, the same
interleaving, both arms built through `--commit`:

| Commit | Walls (s), in run order | Peak anon | UUIDs |
|---|---|---:|---|
| `81f3fbe4` (fix) | 305.3, 304.7 | 3.84 GB | `d6f87532`, `76050327` |
| `8f8ef92a` (before) | 304.6, 304.4 | 3.72 GB | `2aa41b13`, `1f3505e7` |

Both arms run at about 28 of 32 cores. In the fix the way phase grows by
about 6 s (126.5 s against 120.5 s) and the node phase shrinks by about
5.5 s from the cursor seeding, so the walls match. The fix refreshed
244,817 refs in 58,701 ways (the read-only measurement predicted exactly
that), searched 505 way blobs and rewrote 163 only for the refresh; on
this base most way blobs are already rewritten for merge work.
`loc_missing` 0. The refresh probe is 100 MB
(`merge_loc_refresh_probe_bytes`), about the peak-anon difference.

Plain merge (no `--locations-on-ways`, europe indexed + OSC 4715), the
same interleaving: `8f8ef92a` 52.5 s and 25.0 s, `81f3fbe4` 50.7 s and
26.4 s (UUIDs `49d6c2b4`, `2c21e88c`, `040ad80b`, `08bc51e8`); first pair
cold, second warm. No regression outside the noise.

### Missing-node refusal at the barrier (`--locations-on-ways`, commit `8efb71bd`)

Since `8efb71bd` the missing-node refusal is taken at the node-to-way
barrier, and the scanner polls the barrier channel on every held way and
relation header, so a refusal ends its header walk at once. Planet
`altw-keep-untagged` + OSC 4913, bygg, interleaved `--bench 1` cells
against the parent (`03e3792d`, built through `--commit`), no trim
between them:

| Commit | Walls (s), in run order | Barrier at (s) | UUIDs |
|---|---|---|---|
| `03e3792d` (parent) | 299.0, 302.9 | 170.9, 172.8 | `8cb121ef`, `d6178654` |
| `8efb71bd` | 302.7, 300.5 | 173.3, 172.6 | `6b404055`, `c57d4058` |

Run order was parent, change, change, parent. Medians 301.0 s and 301.6 s,
inside the noise; the way phase (barrier to loop end) is 126 to 128 s in
every cell. On the change's cells there is no
`MERGE_SCANNER_BARRIER_WAIT` span: the scanner, still walking way headers
when the drain signalled, took the signal at a poll and skipped its
end-of-input wait. It still releases the held descriptors only at end of
input, as before.

The refusal itself, plain `altw` (untagged nodes dropped, so 4,237,222
of the 6,803,426 nodes the OSC's ways need are missing), one run each,
refused runs are not stored in results.db (brokkr history ids 53910 and
53912, sidecar via `brokkr sidecar dirty` at the time):

| Commit | Refused at (wall) | Barrier at | Scanner end-of-input wait at |
|---|---:|---:|---:|
| `03e3792d` (parent) | 96 s, after the full way phase | 12.5 s | 26.4 s |
| `8efb71bd` | 15.0 s (refusal marker at 14.2 s) | 14.2 s | not reached |

The parent's numbers show why the scanner poll matters here: on this base
the barrier comes about 14 s before the scanner's header walk would have
reached end of input, so a refusal that waited for the walk would have
come at about 26 s.

### Way work starts at publication (`--locations-on-ways`, commit `0f417a44`)

Since `0f417a44` the scanner blocks for the barrier signal at the first
way or relation header and dispatches way work as it walks from there;
before, it held every way and relation descriptor until its header walk
reached end of input. Planet `altw-keep-untagged` + OSC 4913, bygg,
interleaved `--bench 1` cells against the parent (`241be923`, built
through `--commit`), run order parent, change, change, parent, no trim
between them. The idle gap is read from the sidecar: process user time
from the drain's `MERGE_DRAIN_BARRIER_END` marker until it climbs back to
the way-phase rate (about 27 cores).

| Commit | Wall (s) | Barrier end (s) | Idle after it | UUID |
|---|---:|---:|---|---|
| `241be923` (parent) | 298.5 | 171.1 | about 1.7 s at under one core, the scanner still walking way headers | `329708d8` |
| `0f417a44` | 302.8 | 174.8 | one 0.2 s sample at about 5 cores | `bfe11306` |
| `0f417a44` | 300.7 | 174.1 | one 0.2 s sample at about 3 cores | `67230fed` |
| `241be923` (parent) | 301.3 | 173.8 | about 0.2 s: this walk reached end of input before the barrier | `b6a8efbc` |

The parent's gap depends on whether its header walk outruns the node tail;
here it was 1.7 s and 0.2 s (1.3 s and 2.9 s on the `8efb71bd` cells
above). The change leaves only the merge itself and the worker ramp. The
walls do not move: medians 299.9 s and 301.8 s, inside the noise, and the
way phase (barrier end to loop end) is 126.4 to 127.9 s in every cell. The
scanner waits about 0.5 s for the drain at the first way header
(`MERGE_SCANNER_BARRIER_WAIT_START` to `_END`), the node tail.

### Claims checked on decoded blobs (bygg, 2026-10-27, commit `ac482666`)

Since `ac482666` every blob an apply-changes worker decodes is walked
against its claim (count, numeric min and max, OSM-key order) on the loops
that already visit each element, and the `sort` / `cat --dedupe` sweep
tallies each believed overlap-run entry while pushing it. Interleaved
`--bench 1` cells against the parent `43e20723` (built through `--commit`),
run order parent, change, change, parent, bygg, untrimmed:

| Cell | Parent `43e20723` (s) | New `ac482666` (s) | UUIDs, in run order |
|---|---|---|---|
| apply-changes planet, OSC 4913 | 232.9, 236.6 | 234.2, 235.8 | `1ba4bce2`, `6d2dce1e`, `5c8a398d`, `6982b7fc` |
| apply-changes `-l`, `altw-keep-untagged`, OSC 4913 | 303.2, 307.1 | 306.7, 306.2 | `5ddfaca1`, `b6e2d94c`, `df4f9c49`, `8604ba5e` |
| cat --dedupe japan with itself | 14.1, 10.2 | 10.6, 10.3 | `1547090b`, `cf5cc441`, `d62b4f9b`, `c43029dc` |

Medians move by +0.1 % (plain, 234.8 to 235.0 s), +0.4 % (`-l`, 305.2 to
306.5 s) and +2 % on the two warm dedupe cells (10.2 to 10.45 s; the first
parent cell ran cold after the planet cells and is not comparable), inside
the noise band. The worker counters locate the cost where the walk runs:
`merge_streaming_precise_ns` rises from 70.0 and 70.6 to 84.5 and 84.5
core-seconds on the plain merge, and from 118.7 to 140.5 and 142.0 under
`-l`, where every node blob is a candidate and its precise check walks every
node; about 0.5 s and 0.8 s of wall at 29 cores. Rewrite, decompress and
parse time are unchanged, and blob counts are identical. The dedupe cell is
the worst case for the sweep's tally: every one of its 86,070 blobs is
rewritten through the sweep on a believed claim. `sort` was not measured
separately: under a header claiming sorted order an honest file has no
overlap runs, so the believed sweep runs on valid input only for
`cat --dedupe`, and an unsorted-header `sort` sweeps scanned entries, which
the tally skips.

### Claim checks in add-locations-to-ways and time-filter (bygg, 2026-10-27, commits `5587582a` to `2d4e3404`)

Slice 3 holds every blob add-locations-to-ways decodes to its claim and
time-filter to the sort claim. All cells `--bench 1`, interleaved against
the parent `d0aea1d2` (built through `--commit`), bygg, untrimmed. Worker
counters are summed worker elapsed seconds, not CPU; span CPU is user
core-seconds summed over every sidecar segment of the span
(`scripts/sidecar_span_totals.py`; `scripts/altw_cells.py` tabulates the
counters).

**External, planet: the first commits cost CPU, removed before the final
one.** On `5587582a` stage 2's node extraction, where the shared
`ClaimWalk` ran per tuple, took 101.8 and 102.4 worker-seconds against
85.9 and 86.4 (`1bac7795`, `98b44690`, `2fc0c5da`, `e1b62c8a`), and on
`a4ffa7b3` stage 4's node re-encode, the same walk per node, took 720.8 and
727.3 against 608.2 and 633.7 (`7cbfa307`, `d13ba24d`, `7be2bd68`,
`a228bdb2`), the same sign in every pair. Both now use `AscendingTally`
(one raw compare, a store and a count per ID, decisive because external's
node claims are non-negative).

**Final code, planet external, settling sequence.** Earlier final-code
cells were confounded: a parent cell came first after each idle gap (a
rebuild, the test suite), and pass A, which writes 147 GB of id shards,
was 100 to 110 s on those cells and 150 to 200 s on cells run back to
back, with the same bytes written and less CPU. The sequence below was
agreed to settle it: one conditioning run (`56d0a576`, parent), then
parent and change alternating back to back with builds done beforehand.

| Cell, run order | Wall (s) | Pass A (s) | Stage 2 (s) | Streaming (s) | Streaming user core-s | Stage 2 extract (worker-s) | Stage 4 node re-encode (worker-s) |
|---|---|---|---|---|---|---|---|
| `43a495ea` parent | 571.8 | 173.3 | 206.6 | 183.6 | 2630.0 | 86.5 | 626.5 |
| `439cc375` change | 581.7 | 174.6 | 206.4 | 192.6 | 2636.6 | 89.1 | 643.1 |
| `20efc913` parent | 579.0 | 170.9 | 213.8 | 186.1 | 2717.1 | 86.2 | 634.9 |
| `56736441` change | 622.7 | 180.6 | 223.0 | 211.0 | 2656.3 | 89.0 | 629.4 |
| `cc799657` parent | 594.3 | 186.5 | 214.3 | 185.3 | 2626.2 | 86.3 | 618.9 |
| `50823a3d` change | 564.9 | 168.1 | 206.6 | 182.1 | 2749.2 | 91.6 | 626.5 |

Wall medians 579.0 (parent) against 581.7 s (change), +0.5 %, inside the
noise. Pass A, stage 2 and streaming medians move by +0.8 %, -3.4 % and
+3.9 %, and their user CPU by -2 %, +0.3 % and +0.9 %. The one cost the
data resolves is stage 2's node extraction, 86.3 to 89.1 worker-seconds
(+3.2 %, every change cell above every parent), about half a second of
wall at its 6 workers. Stage 4's node re-encode moves +0.5 % on the
medians, unresolved. The slow change cell `56736441` is slow in streaming
(211.0 s against 186.1 s for the parent before it) with 2.2 % less user
CPU, the same bytes read and written, and read and write throughput about
11 % lower; stage 3 reads, the coordinate-payload preads and stage 4's
preads rise 13 to 20 %: an I/O slowdown, not check CPU. Not explained:
stage 1's way scan, which now runs a `ClaimWalk` per way, is lower on the
change in every cell (median 58.2 against 50.9 worker-seconds).

**Sparse, north-america** (auto routes it to sparse here: store estimate
18.98 GB against an 80 % share of a 27.46 GB budget). Run order parent,
change, change, parent: walls 94.9 (cold page cache: its auto estimate walk
took 33 s), 61.1, 53.7 and 67.9 s (`9c85e66a`, `9943a40e`, `ebab1c11`,
`23b988fc`). Pass 1, which now tallies every node tuple, used 75.5, 75.2,
78.3 and 73.8 user core-seconds and took 10.7, 9.6, 9.6 and 10.8 s: CPU
level, and a wall 1.1 s lower on both change cells that the figures do not
explain (its kernel time is 1 to 3 core-seconds lower). Pass 2 used 501,
498, 495 and 486 user core-seconds; its walls (42.3, 41.9, 36.6, 49.8 s)
follow the store's page-cache residency, not CPU.

**time-filter, planet** (snapshot path, one key compare per element):
175.9, 174.9, 178.2 and 175.4 s, run order parent, change, change, parent
(`1cadc624`, `9bcc192d`, `13aaad48`, `176d004e`), medians +0.5 %.

**diff `-c`**, which now counts single-sided runs from the decoded block:
planet does not fit brokkr's 16 MiB stdout capture, so germany with OSC
4705 stands in: 3.2, 3.1, 3.1 and 3.1 s (`d0e29e64`, `bd8e6362`,
`f4204986`, `9bcdff49`).

### OSC-only path (no `--locations-on-ways`, planet, commit `16e3694`)

Apply-changes without `--locations-on-ways` is a structurally
different code path: no node→way coord-fusion barrier, no per-worker
coord slots, no `LocMapHandle`. The scanner classifies blobs against
the OSC ID set and rewrites only the touched blobs; everything else
is `CopyRange` passthrough.

| Compression | Wall | UUID |
|---|---:|---|
| `--compression none` | 274.2 s (4m34s) | `fda9f7a6` |
| `--compression zlib:6` | **462.3 s (7m42s)** | `18b695ed` |
| `--compression zstd:1` | 269.7 s (4m30s) | `3ad57fc5` |

zlib:6 is the outlier at 1.7× zstd:1 wall - the rewritten blobs
re-compress on the writer thread, and with most of planet's blobs
rewritten (43,670 of 50,816 by count in `3ad57fc5`) zlib's serial
deflate becomes the bottleneck. zstd:1 narrowly
beats `none` (parallel-compressed bytes leave the writer faster than
uncompressed bytes through the same pipe). For OSC-only daily-diff
pipelines that don't need osmium-interop output, zstd:1 is the right
default.

## Add-locations-to-ways

The Europe and Planet figures directly below are historical: commit
`69a127f` ran the dense mmap index (16-byte slots, 128 GB of virtual
address space, only touched slots resident), which has since been
removed. The index types that exist now are compared in "Dense vs Sparse
vs External index" and the sections after it.

Commit `69a127f`, plantasjen (30 GB RAM, 8 GB swap).

### Europe (33.6 GB indexed, 4.2B elements)

3.7B nodes read, 149M written, 3.57B dropped. 453M ways, 8.2M relations.
1029 passthrough blobs, 521K decoded. 0 missing locations.

| I/O Mode | Time |
|----------|------|
| Buffered | **2565s** (42m45s) |
| `--direct-io` | 2611s (+2%) |

### Planet (87.7 GB indexed, 11.6B elements)

10.4B nodes read, 285M written, 10.2B dropped. 1.17B ways, 14.1M relations.
452 passthrough blobs, 50K decoded. 0 missing locations.
Output: 88.4 GB (+0.7% from embedded way-node coordinates).

| I/O Mode | Time |
|----------|------|
| Buffered | **5773s** (96m) |

Planet on 30 GB host with 8 GB swap - memory-latency-bound (page faults on
sparse mmap index), not compute-bound. Production host (64 GB RAM) should be
well under an hour.

`--direct-io` provides no benefit for ALTW - workload is compute/memory-bound,
not I/O-bound. Sequential I/O benefits from page cache prefetch.

### Dense vs Sparse vs External index (plantasjen, historical)

The `dense` index type has since been removed (`b70dd8c`): the rank-indexed
flat sparse index beat it at every measured scale and also works where dense
could not. `--index-type dense` now errors. The Dense column below is the
historical measurement from when it existed, kept as a record; the Sparse
column is the pre-rank-flat sparse, so those numbers are also superseded.
Current sparse and external figures are in the sections that follow.

| Dataset | Dense | Sparse | External | Commit |
|---------|-------|--------|----------|--------|
| Denmark (465 MB) | **6.8s** | 14.1s | 14s | `ee9b19f` |
| Japan (2.4 GB) | **42s** | - | - | `b3e8bf7` (node scanner) |
| Europe (33.6 GB) | 2,940s (49m)* | 6,453s (107m) | **400s (6.7 min)** | `3d977a0` |
| Planet (87.7 GB) | 5,773s (96m)* | - | **953s (15.9 min)**, 8.7 GB peak anon | `3d977a0` |

*Dense at Europe scale thrashes on 30 GB host (mmap working set ~16 GB > available
RAM). Japan 42s is with node-only scanner for pass 1 (commit `b3e8bf7`, previously
72s with pipelined PrimitiveBlock). Europe 2,940s is also with node scanner but
mmap thrashing dominates.

*Planet with dense thrashes on 30 GB host (memory-latency-bound).

Dense was fastest when the working set fit in RAM. External uses ~1.6 GB
anon RSS at Europe scale via 4-stage radix join pipeline (node-only wire
scanner for stage 2, scatter buffer for stage 3, sequential reader for
stage 4).

External's anon figures exclude the per-way ref-count sidecar, which
stages 3 and 4 read through a read-only mmap of the scratch file. Its
pages are file-backed and reclaimable: they show in RSS as file pages,
not in peak anon, and under memory pressure they are dropped and
re-read rather than swapped. The anonymous part of that index is one
byte offset per way blob. Before the mmap the whole encoded sidecar
was read into heap and counted in anon, so anon peaks recorded before
the change and after it are not directly comparable on this term.
The blob-order slot-start prefix vector (one `u64` per way blob) is
still anonymous.

Current Europe external baseline on `main` (post-A1 commit `0dc8ae1`):
**270.8 s** (`4m31s`, UUID `0b89f986`, `--bench 1`). A1 (rankless
node-ID bucketed stage 1+2, landed 2026-04-25) cut europe from
291.6 s at `6d71053` (-7.1 %) by replacing the two-pass way scan +
rank index with single-pass IdRecord emission and a streaming
merge-walk.

**Historical crossover point** (dense no longer exists): between Japan
(2.4 GB, dense 2x faster) and Europe (33.6 GB, external 7.4x faster). At
Europe scale, dense's mmap working set (~16 GB) exceeded available RAM,
causing thrashing. External's sequential I/O stays bounded.

### Sparse `MADV_RANDOM` on the coord store (europe, plantasjen, 2026-07-16, commit `25cb0df`)

Pass 2's random lookups no longer pay kernel readahead speculation: the
store mapping is advised `MADV_RANDOM` once pass 1 has populated it.
Adjudicated on the matched quiet pair (cells adjacent, same day,
post-trim). Four further same-day cells (`53763e55`, `7027481c`,
`9a72b914`, `6b9942e5`) ran under host load and adjudicate nothing
arm-vs-arm on wall, though every probe cell, the memory-pressured one
included, read fewer pass-2 bytes than the best baseline cell:

| | probe `3027542f` (`25cb0df`) | baseline `c45b45ee` (`13910d2`) | delta |
|---|---:|---:|---:|
| pass 2 minus schedule walk | 231.2 s | 260.8 s | **-11.3 %** |
| pass-2 disk read | 260 GB | 600 GB | **-57 %** |
| pass-2 KB per major fault | ~4.8 | ~43 | mechanism confirmed |
| total wall | **359 s** | 384 s | -6.5 % |

**359 s (`3027542f`) is the current europe sparse reference** (quiet
host, post-trim, `--bench 1`), superseding 363.1 s (`4ac11326`) as
bookkeeping only - the cells are not comparable. Caveat carried from
the adjudication: under severe external memory pressure the
no-readahead trade multiplies synchronous fault round-trips (104 M vs
54 M faults for identical work in the contaminated cell) and the wall
win can invert; inputs whose store fits comfortably in RAM are
unaffected either way.

### Auto routing threshold: sparse store versus page-cache budget (plantasjen, commit `dcc445e`, 2026-07-14)

The measurements behind `--index-type auto` and `AUTO_STORE_BUDGET_PCT`
(the routing rule itself is in [pipeline.md](pipeline.md)). Sparse versus
external walls:

| Dataset | sparse store | sparse | external | winner |
|---|---:|---:|---:|---|
| germany | 3.32 GB | 25.7 s | 25.2 s | tie |
| north-america | 18.75 GB | **116.4 s** | 158.3 s | sparse, -26 % |
| europe | 28.94 GB | 363.1 s (`4ac11326`) | **285.7 s** (`cdfa9453`) | external |

The germany and north-america rows are matched `--bench 1` pairs. The
europe row is not matched (sparse `--bench 1`, external `--bench 3`), so
its margin is overstated; the ordering is not in doubt. The walls are
non-monotonic in store size, so the threshold is set from where sparse
pass 2 goes nonlinear rather than from where the walls cross. Per-ref
cost of sparse pass 2's random store lookups (`way_reframe_ms`
cumulative over refs):

| Dataset | store | refs | per ref |
|---|---:|---:|---:|
| north-america | 18.75 GB | 2.60 B | 0.145 us |
| europe | 28.94 GB | 4.37 B | **0.772 us** |

5.3x the per-ref cost for 1.68x the refs. With roughly 2-3 GB of anon
held outside the store, the measuring host's page-cache budget was about
25 GB: north-america's store sat near 75 % of it and stayed linear,
europe's near 116 % and thrashed. These two points bracket the knee
without locating it; 80 % is a choice inside the bracket biased toward
external, because routing a smallish input to external wastes seconds
while routing an oversized one to sparse costs minutes. The threshold is
relative to runtime available memory, never a byte constant fitted to
this host.

### Compression axis (Europe external, plantasjen, `--bench 1`)

| Compression | Wall | Peak anon | UUID | Commit | Δ vs zlib:6 |
|---|---:|---:|---|---|---:|
| `zlib:6` (default) | 270.8 s | n/a | `0b89f986` | `0dc8ae1` (post-A1) | - |
| `none` | **246.8 s** (`4m07s`) | 6.5 GB | `16c35911` | `4fc8e35` | -24.0 s, -8.9 % |
| `zstd:1` | **233.3 s** (`3m53s`) | 6.6 GB | `e2fba1bf` | `4fc8e35` | -37.5 s, -13.9 % |

The zlib:6 row was measured at an earlier commit (`0dc8ae1`); the
none/zstd:1 rows pin the compression-axis comparison to `4fc8e35`
(2026-04-27 overnight) but the cross-commit zlib:6 reference is not
co-pinned. Order of magnitude matches the prior 2026-04-14 finding
(419 s zlib:6 → 379 s zstd:1, -9.5 %, at the older `f3c53a34`/`66e43a11`
baselines): zstd:1 wins by relieving consumer/compression saturation
in stage 4, with similar output size.

> **This table is Europe-only and does not generalise to planet.** The
> 2026-07-14 planet cell (`0e9d93cc` vs `ed5dd6c5`, both `dcc445e`)
> found the planet streaming phase is *not* compression-bound: -80 % of
> compression CPU bought <=5 % of streaming wall, and the constraint
> moved to stage-3 -> stage-4 payload readiness instead. The "relieving
> consumer/compression saturation" mechanism above is real at europe and
> absent at planet. See the planet compression-ceiling callout below the
> planet baselines table before quoting -13.9 % at any other scale.

### Current planet baselines (plantasjen)

The consolidated headline table. All rows `--bench 1` unless noted.
Rows marked `dcc445e` were re-baselined in the 2026-07-14 overnight
suite; the rest still carry their older commit in the Notes column.

> **Read the April-to-July deltas as environment, not code.** The
> 2026-07-14 suite proved the host drifted ~+8 % on byte-identical code
> (`16e3694` ALTW: 546.0 s in April, 589.9 s today - see the ALTW drift
> verdict below). Every pre-July baseline on this page is therefore ~8 %
> optimistic relative to today's drive, and a "+11 % regression" against
> one is not a finding. Commands that came back *faster* despite that
> headwind (apply-changes, build-geocode-index) are understated wins.

| Command | Mode | Wall | UUID | Notes |
|---|---|---:|---|---|
| cat (indexdata generation) | `--bench 1` | **87.2 s** | `f2c1e220` | commit `dcc445e`, 2026-07-14; was 86.5 s `5d90623f` (`aee7727`). A same-day re-run at the very end of a second suite measured **108.1 s** (`cdf95e0d`, `fb743f6`, +24 %) - identical code, exhausted drive. **Treat that spread as the honest error bar on any single planet `cat` number**, not as a regression |
| cat --type way | `--bench 3` | 45.3 s | `2fe62148` | |
| cat --type relation | `--bench 1` | 47.7 s | `fba6e13e` | |
| cat --clean version | `--bench 1` | **333.8 s (5m34s)** | `f2315551` (4fc8e35, 2026-04-27 overnight) | `parallel_classify_phase` + reorder-buffer streaming, 750 MB peak anon |
| cat --dedupe | `--bench 1` | **7,981 s (133m)** | `1794f8a6` | single-threaded MERGEPBF path - see callout below |
| check --refs | `--bench 1` | **60.2 s** | `f75bedc6` | commit `dcc445e`, 2026-07-14; was 53.8 s `7d9f5dfd`. +11.9 % on a single sample, inside the ~8 % drift band. Briefly re-opened 2026-07-14 on a `4776b92` suspicion that the getparents phase split then killed; **stays closed**. If ever re-examined, check the schedule-phase disk read first - check-refs walks headers too |
| check --ids (streaming, default) | `--bench 1` | **56.6 s** | `55e87d85` | commit `dcc445e`, 2026-07-14; was 56.4 s `b1fc4d2e` (4fc8e35) - flat |
| check --ids --full | `--bench 1` | **63.2 s** | post-`01c67da` | |
| getid (include mode) | `--bench 1` | **6.8 s cold / 0.2 s warm** | `264d9dbf` / `12e74756` | dispatch landing `19d3a62`, 2026-07-11; cache-state dominated, see ADR-0006 gates below |
| getid --invert | `--bench 1` | 91.0 s | `40f5bd52` | |
| getparents | `--bench 3` | **22.8 s** | `39570d10` | commit `fb743f6`, 2026-07-14. Matched same-day pair vs `--commit 2306fd9` 24.4 s (`da60663d`): **HEAD is 6.6 % faster**. The 19.0 s `a7c064eb` figure is retired - it was a warm-header-walk artifact, see flag below |
| inspect default (index-only) | `--bench 1` | **6.5 s** | `c146f2bb` | |
| inspect --nodes `-j 16` | `--bench 1` | **49.4 s** | post-`01c67da` | |
| inspect --tags `-j 16` | `--bench 1` | **168.3 s** | `9d741341` / post-`01c67da` | |
| inspect --tags --type node | `--bench 1` | 71.3 s | `047ac2f9` | |
| inspect --tags --type way | `--bench 1` | 82.9 s | `959bda7c` | |
| inspect --tags --type relation | `--bench 1` | 8.8 s | `8daf5f04` | |
| inspect --extended | `--bench 1` | **820.7 s (13m41s)** | `19db1512` | full decode + extended counters |
| sort (already-sorted input) | `--bench 1` | **147.0 s** | `e278faff` | commit `dcc445e`, 2026-07-14; was 132.3 s `b9c10a41` (`16e3694`). Regression flag CLOSED - see below |
| sort `--io-uring` | `--bench 1` | 126.8 s | `9ce80125` (`16e3694`) | **not re-pinned**: host `RLIMIT_MEMLOCK` soft limit is 8 MB, io_uring preflight needs >= 16 MB. Raise via `/etc/security/limits.conf` and re-queue |
| tags-filter -R | `--bench 1` | 51.8 s | `cf116a6b` | |
| tags-filter (transitive) | `--bench 1` | **108.2 s** | `7e4301f9` | |
| tags-filter --invert-match | `--bench 1` | 461.2 s (7m41s) | `6665605a` | 4.3× the match path; ~99 % of ways kept |
| tags-filter --remove-tags | `--bench 1` | 111.8 s | `44d96d0a` | two-pass with tag-stripping in pass 2 |
| tags-filter --input-kind osc (osc 4913) | `--bench 1` | 6.2 s | `37f360d2` | OSC parse + filter only |
| extract --simple (Europe bbox) | `--bench 1` | **221.9 s** | `e43bb19f` | |
| extract --complete (Europe bbox) | `--bench 1` | **222.7 s** | `91fd90b4` | |
| extract --smart (Europe bbox) | `--bench 1` | 267.5 s | `07dcdae3` | |
| multi-extract --simple (5 regions, Europe bbox) | `--bench 1` | **883.6 s** | `68cecf88` | |
| multi-extract --smart (5 regions, Europe bbox) | `--bench 1` | 837.6 s | `2c842414` | |
| add-locations-to-ways `--index-type external` | `--bench 3` | **592.2 s** | `b65fcad0` | commit `dcc445e`, 2026-07-14, first cell of a suite on a rested drive - **this is a best case, not a typical wall**. Six same-day `--bench 1` cells at `fb743f6` measured **621.8 - 655.0 s** on identical code as the drive tired (`4ff288fb` / `8cc542eb` / `2205c7d5` flag-OFF). Quote the range, not a point |
| add-locations-to-ways `--index-type external --compression zstd:1` | `--bench 1` | 648.4 s | `0e9d93cc` | commit `dcc445e`, 2026-07-14. **Total wall is drift-confounded; the finding is the phase split** - see the planet compression ceiling callout below |
| apply-changes (daily diff, `--osc-seq 4920`) | `--bench 1` | **465.7 s** | `aaed129a` | commit `dcc445e`, 2026-07-14; was 756.3 s `8e940f71`. **-38.4 % despite the ~8 % drift headwind - understated win, unattributed** |
| repack (default cap, planet -> planet) | `--bench 1` | **374.6 s** | `33f2c448` | commit `dcc445e`, 2026-07-14; was 377.5 s `8027765b` (`8c1cf03`) - flat |
| time-filter (snapshot, cutoff 2024-01-01) | `--bench 1` | **263.8 s** | `8781ad14` | commit `dcc445e`, 2026-07-14; was ~270 s (`83183fb`-era) - flat |
| tags-filter (default two-pass, `w/highway=primary`) | `--bench 1` | **115.2 s** | `cd401093` | commit `dcc445e`, 2026-07-14; was ~117 s (`cf116a6b`-era) - flat |
| renumber | `--bench 3` | **191.1 s** | `43041dd4` | commit `dcc445e`, 2026-07-14; was 204.5 s `abd74459`. +10 s flag CLOSED - see below |
| diff-snapshots text `-j 16` | `--bench 1` | **227.5 s** | `22a5eb55` | |
| diff-snapshots osc `-j 16` | `--bench 1` | **293.8 s** | `cdcaa4f1` | |
| build-geocode-index | `--bench 1` | **347.0 s** | `57878b0f` | commit `dcc445e`, 2026-07-14; was 424.8 s `2b412af4` (2026-04-26). **-18.3 % despite the ~8 % drift headwind - understated win, unattributed** |
| merge-changes (planet, `--osc-seq 4913`, 1-OSC) | `--bench 1` | 44.2 s | `941a5784` (2026-04-28) | not re-pinned in the 2026-07-14 suite |
| merge-changes (planet, `--osc-range 4914..4920`, 7-OSC) | `--bench 1` | **53.8 s** | `f4de3f8d` | commit `dcc445e`, 2026-07-14; was 54.7 s `b6e964cc` - flat. Parallel-drain landed earlier (was 267.2 s `bef0f1fa`) |
| merge-changes (planet, `--osc-range 4914..4920 --simplify`, 7-OSC) | `--bench 1` | **73.7 s** | `3e3ef119` (2026-04-28) | parallel parse + parallel write_simplified landed (was 262.2 s `c0d140b6`) |

> **Sort `+6-7 %` regression flag - CLOSED 2026-07-14, not a code
> regression.** The flag tracked default and `--io-uring` sort drifting
> slower at `16e3694` vs the 2026-04-24 `1f97fae` / `68e1ba0`
> baselines. The 2026-07-14 suite settles it from the environment side:
> ALTW's `16e3694` binary re-ran at +8.0 % on byte-identical code, so
> the host itself moved by more than the flagged delta. Sort's fresh
> `--bench 1` (`e278faff`, 147.0 s) is +11.1 % on the same single-sample
> basis - fully absorbed by that band plus normal single-shot noise on a
> sub-150-s wall. The `4fc8e35` hotpath (`d64932d2`, **115.4 s** total;
> 108.6 s / 94 % in `pbfhogg::write::writer::flush`, 6.77 s / 6 % in
> `build_blob_index`) already sat *below* both baselines on the same
> writer-flush-dominated mix and pointed the same way. The truncation
> commits (`436998b`, `12699db`) are exonerated. **Do not chase this;
> re-open only against a matched-commit `--bench 3` pair.**

> **ALTW drift flag - CLOSED 2026-07-14: drive state, not code.** The
> matched `--bench 3` pair settles it on the same-commit comparison:
> `16e3694` measured **546.0 s in April and 589.9 s today**
> (`dc62a437`), +8.0 % on byte-identical code. HEAD is 592.2 s
> (`b65fcad0`), 0.4 % away - and conservative, since the April cell ran
> late in the suite on a tired drive and still matched rested HEAD.
> No bisect needed; the 546.0 s baseline is retired, not regressed
> against. The mechanism was caught inside the same suite: the zstd:1
> (`0e9d93cc`) and zlib:6 (`ed5dd6c5`) cells are the same binary an hour
> apart, `--compression` never touches stage 1, both wrote **exactly**
> 149,225,518,932 bytes of id shards - and `s1a_id_shard_write_ms` was
> 1140.7 s vs 1495.6 s cum, **+31 % write time on identical bytes**.
> Drive is 87 % full (469 G free of 3.6 T) and each ALTW planet run
> pushes ~705 GB of scratch through it. **Planet single-sample walls on
> this host carry a ~5-8 % environmental band and run order within a
> suite is a real variable.**

> **Drive health + trim state (checked 2026-07-16).** Follow-up to the
> drift verdict, from a SMART + fstrim pass on the 990 PRO:
>
> - **The drive is healthy. "Drive state" throughout these docs means
>   trim debt and garbage collection on a near-full consumer NVMe -
>   normal firmware behaviour - never hardware degradation.**
>   `smartctl -x /dev/nvme0`: 0 media and data integrity errors, 0
>   error-log entries, 100 % available spare, 4 % percentage-used,
>   142 TB written of the 4 TB 990 PRO's 2400 TBW rating (~6 % of
>   rated endurance; roughly 3,200 more ALTW planet runs at
>   ~705 GB/run). The 970 EVO Plus is likewise clean (its 2,520
>   error-log entries are all benign "Invalid Field in Command" from
>   tooling probing unsupported NVMe 1.3 log pages).
> - **A manual `fstrim -av` reclaimed 518 GiB** on the data drive
>   (`/dev/nvme0n1p1`, 2026-07-16). The weekly `fstrim.timer` cadence
>   cannot keep up with ~705 GB of scratch churn per ALTW planet run:
>   dead blocks accumulate for up to a week, and the controller saw
>   ~97 % namespace utilization (3.89 of 4.00 TB allocated) during the
>   2026-07-14 suite even though `df` reported 469 G free. That trim
>   debt - GC shuffling dead scratch blocks with almost no free flash
>   to work with - is the mechanism behind the +31 % same-bytes write
>   drift.
>
> Consequences: (1) **trim before any matched A/B suite** (and between
> cells of a long one) - it does not replace interleaving, but it
> shrinks the band interleaving has to cancel; (2) **every 2026-07-14
> planet baseline above was measured under worst-case trim debt** -
> post-trim runs may come in systematically faster for drive-state
> reasons, so a new-vs-2026-07-14 improvement needs a same-day matched
> pair before it is credited to code. The ~5-8 % band stands until
> re-measured post-trim.

> **`--inject-prepass` A/B - SETTLED 2026-07-14: cost is under 1 % at
> planet. Stop measuring this.** Six interleaved `--bench 1` cells at
> `fb743f6`, ON leading, alternating:
>
> | pair | ON | OFF | delta |
> |---|---:|---:|---:|
> | 1 | 627.4 s (`8267419a`) | 621.8 s (`4ff288fb`) | ON +0.90 % |
> | 2 | 644.6 s (`8333cbca`) | 645.4 s (`8cc542eb`) | ON -0.12 % |
> | 3 | 646.3 s (`2a5b109a`) | 655.0 s (`2205c7d5`) | ON -1.33 % |
>
> **Median ON 644.6 s vs OFF 645.4 s = 0.12 %, sign flipping across all
> three pairs.** ADR-0007's planet regression gate closes for good;
> record the bound as **<1 %**.
>
> Why the two earlier attempts failed, and the method that worked: drift
> across this suite was large and monotonic (walls climb 627 -> 655 s,
> +4.5 % in an hour) and is proven on byte-identical work - first cell
> `s1a_id_shard_write_ms` 1,174,295 ms vs last cell 1,515,132 ms, **+29 %
> over exactly 149,225,518,932 shard bytes both times**. A blocked pair
> hands that drift to whichever arm runs last: that is the +5.5 %
> (2026-07-14 blocked `--bench 3`) and the -5.3 % (2026-07-13
> `--bench 1`) - same question, opposite signs, both pure run order.
> Interleaving alternating `--bench 1` cells samples both arms across the
> same drift and cancels it. **Matched sample counts are necessary but
> not sufficient; `--bench N` is best-of-N within one cell and cannot
> cancel between-cell drift.**
> Injection counters (2026-07-13, first end-to-end exercise, all
> plausible): `altw_member_ways` 37.2 M, `altw_pinned_refs` 2.63 B (21 %
> of the 12.44 B way refs), `altw_field20_ways_emitted` 535.3 M (46 % of
> 1.166 B way messages), `altw_field5_bytes` 145.8 MB.

> **Planet ALTW streaming is NOT compression-bound (2026-07-14,
> `dcc445e`).** The single planet compression cell licensed by the
> do-not-retry reopen condition has been spent, and it inverted the
> prior model. Matched HEAD `--bench 1` pair, zstd:1 (`0e9d93cc`) vs
> zlib:6 (`ed5dd6c5`): `writer_compress_ns` **3855.2 s -> 767.8 s cum
> (-80 %, -3087 core-seconds)** while streaming wall (s3+s4) moved only
> **252.8 s -> 240.0 s (-5.1 %)**. Deleting 80 % of compression CPU
> bought at most 12.8 s. Under zstd:1 the constraint moves to stage-3 ->
> stage-4 payload readiness (`s4_readiness_wait_ms` 667.0 -> 1395.2 s
> cum, ~63 s per decode thread of a 240 s phase) while `s4_send_ms`
> collapses 705.2 -> 59.3 s cum; output grows 91.17 -> 95.20 GB. **The
> europe -13.9 % zstd:1 result does not transfer to planet** - a
> europe measurement is not evidence about planet's regime. Read the
> direction, not the magnitude: total wall says zstd:1 is *worse*
> (648.4 s vs 628.2 s) purely because +31.7 s of drive drift in the
> codec-independent stages 1+2 swamped the streaming win, which is also
> the cleanest illustration on file of why planet single-sample
> total-wall comparisons cannot be trusted here. Consequence: the
> ceiling-gated items (ALTW S2 DenseNodes wire filter, the
> A2/output-executor family) lose their gating premise - freed stage-4
> CPU has nowhere to go under *either* codec. The measured streaming
> constraint is the stage-3 -> stage-4 payload handoff, which is where
> any further planet streaming work has to aim.

> **`cat --dedupe` planet 133-minute wall.** Single `MERGEPBF` phase,
> peak anon RSS only 1.4 GB, avg cores 1.3 - the path was essentially
> single-threaded for the full 87 GB input. `cat --type way`
> passthrough is 45.3 s on the same input, so the dedupe overhead is
> ~175× the passthrough wall. The bench merges the planet with itself
> (brokkr passes the same path twice), so every blob sits in a two-blob
> overlap group and the whole file is decoded, heap-merged and
> re-encoded on one thread; it never was a `BTreeMap` pass, as this
> callout used to say. Since that run, `cat --dedupe` shares `sort`'s
> engine (`src/commands/blob_merge.rs`): pass 1 is the `HeaderWalker`
> walk, passthrough is coalesced `copy_file_range`, and each overlap
> group is swept on a worker thread with bounded streaming. **Unmeasured** -
> the 133-minute row predates all three and needs a fresh cell. A fresh
> cell has more to read than that row: `MERGEPBF_*` now nests
> `MERGEPBF_PASS1_*`, `MERGEPBF_PASS2_*`, `MERGEPBF_WRITE_LOOP_*` and
> `MERGEPBF_FLUSH_*`, the `cat_dedupe_*` counters give the plan's and the
> write's blob, run and duplicate counts, and `--stalls` attributes pass 2:
> `ordered_pool_result_recv` is the write loop waiting on a sweep,
> `ordered_pool_result_send` a sweep waiting on the write loop, and
> `ordered_pool_job_lock` plus `ordered_pool_job_recv` sweep workers idle
> for want of an issued run.

> **Renumber `+10 s` - CLOSED 2026-07-14, no regression.** The matched
> `--bench 3` pair the flag asked for has run: HEAD **191.1 s**
> (`43041dd4`) vs `--commit cb99106` **200.4 s** (`9755cafc`), same day.
> HEAD is faster than the pre-drift commit, and also faster than that
> commit's own historical 194 s. The `cb99106` re-run landing at 200.4 s
> vs its historical 194 s (+3.3 %) is the same environmental drift seen
> everywhere else in this suite. The 204.5 s `abd74459` figure is retired.
>
> **Read the direction, not the 4.6 % gap.** The two cells ran 2h47m
> apart (HEAD 06:10, `cb99106` 08:57) because the suite grouped worktree
> cells under what turned out to be an **obsolete** build-thrash rule
> (brokkr has isolated worktree target dirs since `38e20a6`, 2026-07-07 -
> see AGENTS.md). So `cb99106` was measured on a much more tired drive
> and its 200.4 s is inflated; matched, the two are probably near-equal.
> The conclusion is safe in the conservative direction - the old commit
> got the *worse* conditions and still did not beat HEAD - but the
> magnitude is not a real 4.6 % code win. Do not cite it as one.

> **`apply-changes` parallel_writer join A/B - CLOSED 2026-10-21 (bygg):
> no measurable regression; the +45 % was drive state.** Question: did
> `548395f7` (parallel_writer joins every pool thread before returning)
> regress `apply-changes` against its parent `f2321c21`? Europe,
> `--osc-seq 4715`, default output (parallel_writer), `--bench 1` cells.
>
> | round | cells (walls) | read |
> |---|---|---|
> | untrimmed, mixed builds (HEAD from main-tree `target/`, baseline via `--commit` worktree) | before `04947db2` 35.2 s / after `00826907` 50.6 s; after `11b2f1b2` 63.3 s / before `a63a61b2` 43.3 s | looks like +45 % |
> | untrimmed, both arms via `--commit` worktrees | after `d628550e` 51.6 s, before `d14c2ab8` 59.8 s, before `d4410440` 65.7 s, after `383efa61` 69.0 s | monotonic slowdown by position, either commit |
> | after `sudo fstrim -av`, alternating pairs | before `f30a2af9` 51.9 / after `0b5c18c4` 57.1; after `90bb07ee` 61.7 / before `c464569e` 63.1; before `5b392e5c` 56.3 / after `08cf9360` 62.2; after `ea5c26b1` 63.4 / before `540cd0db` 61.4 | second cell of every pair is ~2-3 s slower; order-corrected effect ~ +3 s, within noise |
>
> **The same binary spanned 35-66 s over the session.** Europe
> `apply-changes` moves ~35 GB in and ~35 GB out, enough to degrade the
> drive within one session even immediately after a trim, so successive
> cells slow by position, not by commit.
>
> **The stall-counter flip was a false mechanism.** The first pairs
> showed the counters change shape (`brokkr sidecar <uuid> --stalls`):
> `a63a61b2` (before, 43.0 s) `writer_pipeline_send` 14.7 s,
> `writer_recv` 23.6 s, `merge_drain_reorder_gap` 3.9 s versus
> `11b2f1b2` (after, 63 s) 0.07 s, 59.1 s, 13.3 s. That reads as "the
> join moved the writer's blocking point", but the other pair
> (`04947db2` before 35.0 s: send 0.08 s, recv 32.4 s, reorder 6.0 s;
> `00826907` after 50.0 s: send 0.08 s, recv 47.8 s, reorder 12.0 s)
> shows the same shape on both commits, with recv and reorder-gap just
> scaling with wall. `writer_recv` is 92-94 % of wall in every cell but
> the fast `a63a61b2` one: the writer is starved by a slower read/merge
> side, which is what a slow drive looks like. A counter-shape change
> between two cells that differ in drive state is not evidence of a code
> cause.
>
> **Page-cache-resident cross-check (germany, `--bench 3`, input fits in
> RAM, so drive state drops out).** Before: `8d80fefa` 3.2 / 3.1 / 3.6 s,
> `62f5a663` 3.5 / 3.9 / 3.7 s. After: `4b869314` 3.3 / 4.2 / 3.2 s,
> `e1d9bdb3` 3.3 / 3.7 / 3.2 s. Median 3.55 s vs 3.3 s: no regression
> (after is nominally faster).
>
> Lessons: (1) for a hot-path change, run a page-cache-resident dataset
> (germany) first - it isolates CPU and sync cost from drive state, and
> here it settled the question in under a minute; (2) europe
> `apply-changes` degrades the drive in-session even right after
> `fstrim`, so cell position dominates and only alternating `--bench 1`
> pairs with order correction are readable; (3) compare arms built the
> same way - HEAD from the main tree against a baseline from a `--commit`
> worktree adds a confound, so use `--commit` for both; (4) do not read a
> stall-counter shape flip as mechanism until the same flip survives a
> same-wall comparison.

> **Library-use B4 (executor and cooperative permit wait) - neutral,
> 2026-10-22 (bygg).** Question: does routing the writer's per-block
> framing through `Executor::spawn` and the permit wait through
> `recv_permit` cost anything on the CLI path? Germany indexed,
> interleaved `--bench 1` pairs, both arms via `--commit` worktrees. The
> B3 arm is `2416a0cb` (B1 and B2): B3 changes no code these two commands
> run under the CLI host (its eviction stays on), and the bricks were not
> yet committed, so the B4 arm is `cbce533a`, a snapshot commit of the
> B3-plus-B4 working tree on `2416a0cb`. `sudo fstrim -av` could not run
> (no passwordless sudo); germany's input stays page-cache resident, so
> the pairs read CPU and synchronization cost.
>
> | command | pair | B3 cell (wall) | B4 cell (wall) | delta |
> |---|---|---|---|---|
> | renumber | 1 | `c5e5aa39` 12401 ms | `f56af7b9` 6601 ms | void, cold cache |
> | renumber | 2 | `6ea722e2` 6301 ms | `30be229a` 6301 ms | +0.0 % |
> | renumber | 3 | `ddf1b865` 6401 ms | `d69725c7` 6201 ms | -3.1 % |
> | renumber | 1, rerun | `4f7c25bb` 6401 ms | `434a7cad` 6601 ms | +3.1 % |
> | repack | 1 | `b619a6f8` 17401 ms | `87f88b51` 17801 ms | +2.3 % |
> | repack | 2 | `1d9fd437` 17502 ms | `351ab291` 17701 ms | +1.1 % |
> | repack | 3 | `b07798a3` 17601 ms | `6d58956e` 17401 ms | -1.1 % |
>
> Renumber's first B3 cell was the session's first germany read
> (`brokkr sidecar --compare c5e5aa39 f56af7b9`: 482 MB, 2087 MB and
> 1809 MB of disk read in its first three phases, none in the B4 cell),
> so that pair was rerun once in the same order and the rerun replaces it.
> Medians: renumber +0.0 % (B3 spread over its three warm cells 1.6 %),
> repack +1.1 % (B3 spread 1.1 %); both within the +2.0 % keep bound.
> `writer_coop_wait_ns` read 0 on `30be229a` and `351ab291`: no CLI path
> drives a `PbfWriter` from a rayon worker. Every wall in the set sits
> within a millisecond of a 100 ms step, so single-pair deltas on renumber
> are one or two steps of that granularity; read the medians, not the
> pairs.

> **Library-use cumulative arc (B1 to B5) - neutral, 2026-10-22 (bygg).**
> `fa94c6c7` (the baseline every library-use identity gate uses) against
> `46675558`, a snapshot commit of the B1-to-B5 tree on `2416a0cb` (the
> bricks were not yet committed). Germany indexed, interleaved `--bench 1` pairs in the same
> cell order as the B4 pair, both arms via `--commit` worktrees, no
> `fstrim` (no passwordless sudo), input page-cache resident.
>
> | command | pair | `fa94c6c7` cell (wall) | B5 cell (wall) | delta |
> |---|---|---|---|---|
> | renumber | 1 | `c5566af6` 6501 ms | `065a72db` 6301 ms | -3.1 % |
> | renumber | 2 | `c815474b` 6501 ms | `3cc4753a` 6501 ms | +0.0 % |
> | renumber | 3 | `44fb7775` 6401 ms | `66cdec33` 6301 ms | -1.6 % |
> | repack | 1 | `1777f885` 17401 ms | `399f5526` 17501 ms | +0.6 % |
> | repack | 2 | `106a67b2` 17601 ms | `2c4cccf9` 17501 ms | -0.6 % |
> | repack | 3 | `28106592` 17701 ms | `f174c9f0` 17501 ms | -1.1 % |
> | cat --clean | 1 | `00560867` 17301 ms | `888814c0` 16901 ms | -2.3 % |
> | cat --clean | 2 | `69e935e1` 16601 ms | `8b073aad` 17401 ms | +4.8 % |
> | cat --clean | 3 | `74188a34` 17401 ms | `759654b0` 17701 ms | +1.7 % |
>
> Medians: renumber -1.6 % (`fa94c6c7` spread 1.5 %), repack -0.6 %
> (spread 1.7 %), cat --clean +1.7 % (spread 4.6 %). All three are within
> the +2.0 % keep bound, so no brick needed bisecting. cat --clean's
> baseline arm alone spans 16.6 to 17.4 s, wider than the bound, and two
> of its three deltas are positive; it is the command to re-pair first if
> a later change touches the classify or framing path.

### Blob-count threshold dispatch landing gates (ADR-0006, plantasjen, 2026-07-11)

getparents and getid include mode dispatch between the pread header
walker and a full-file scan at an estimated 150,000 OSMData blobs
([`decisions/0006`](../decisions/0006-blob-count-threshold-dispatch.md)).
Landing commits `3adb44c` (getparents), `2306fd9` (getid), `dad28de`
(batch-parallel classify fix), `19d3a62` (getid streaming-arm fix).
Gate cells, all measured at HEAD of the day:

| cell | arm (estimate) | wall | UUID | reference (2026-07-10) | verdict |
|---|---|---:|---|---:|---|
| getparents planet primary | Walker (36,063) | **19.0 s** | `a7c064eb` | 23.5 s walker | pass at the time; **now 22.2 s bench 3 (`ca49bcdf`, `dcc445e`) - see flag below** |
| getparents europe `--bench` | FullScan (458,132) | **22.2 s** | `9f8602a2` | 26.4 s scan | pass |
| getparents planet 8k | FullScan (899,866) | 62.0 s | `595e8d7e` | 52.8 s scan (`2b3e496e`) | pass by same-day A/B: `68e1ba0` re-run today 69.4 s (`0e2c2313`), HEAD -11 % |
| getid planet primary | Walker (36,063) | 6.8 s cold / **0.2 s warm** | `264d9dbf` / `12e74756` | 6.1 s walker | pass; cache-state dominated |
| getid europe `--bench` | FullScan (458,132) | **17.6 s** | `6b9ad93c` | 17.9 s scan | pass |
| getid planet 8k | FullScan (899,866) | 48.6 s | `ddf6fed4` | 33.2 s scan (`c0d89d8f`) | pass by same-day A/B: `51c662e` re-run today 48.2 s (`80e726bf`), HEAD +0.8 % |

Disk read on the 8k FullScan cells is the whole file (~96 GB - the
kind filter and prescreen skip decompression, not bytes); the walker
cells read only headers plus matching bodies. The 8k profiles confirm
the mechanism: no schedule/walk phase on the FullScan arm.

> **getparents planet-primary `+16.8 %` flag - CLOSED 2026-07-14 the
> same day it was opened: header-walk cache state, not code.** The phase
> split settles it without needing a bench cell:
>
> | phase | `a7c064eb` 19.0 s | `ca49bcdf` 22.2 s | delta |
> |---|---:|---:|---:|
> | schedule walk | **97 ms**, 0 kB read | **4457 ms**, 405 MB read | **+4.36 s** |
> | decode | 18.857 s, 29.36 GB, 19.3 cores | 17.672 s, 27.85 GB, 20.1 cores | **-1.19 s** |
>
> Net +3.17 s = the whole +3.2 s "regression". **The 19.0 s baseline read
> ZERO BYTES on its schedule walk** - the headers were already in page
> cache from whatever ran before it on 2026-07-11. Today's walked them
> cold (405 MB, 59,838 preads, 0.1 avg cores). `HeaderWalker` uses
> `fadvise(RANDOM)`, so a cold walk is ~88 us/blob and a warm one is
> free: a ~45x swing on identical code. **The decode phase, meanwhile,
> got 6.3 % FASTER.** 19.0 s is the anomaly; 22.2 s is the honest
> cold-cache number. Retire 19.0 s.
>
> Independent corroboration: `21ed8d7c` (getparents planet `--bench 3` at
> `a65cecc`, 2026-07-12, 21.7 GB RAM) measured **25.5 s** - so the series
> runs 19.0 -> 25.5 -> 22.2 across three days with 24.2 / 21.7 / 28.9 GB
> of available memory. Non-monotonic, tracking RAM and preceding
> workload, not commits.
>
> **`4776b92` ("make WILLNEED classify prefetch unconditional") is
> EXONERATED**, twice over: it only touches the decode phase, which
> improved; and `21ed8d7c` sits *before* it in history while measuring
> slower than HEAD. The suspicion below was raised from its commit
> message and killed by the phase split - recorded because the reasoning
> was seductive and wrong.
>
> **The matched same-day pair ran and confirms it (2026-07-14 10:24
> suite):** HEAD **22.8 s** (`39570d10`) vs `--commit 2306fd9` **24.4 s**
> (`da60663d`), `--bench 3` each. **HEAD is 6.6 % FASTER than the commit
> the 19.0 s came from.** Phase split, cache-matched (both cold):
>
> | | schedule walk | decode | wall |
> |---|---:|---:|---:|
> | HEAD run 0 | 4773 ms | 17905 ms | 22678 ms |
> | HEAD run 1 | 4791 ms | 17860 ms | 22651 ms |
> | `2306fd9` run 0 | **6381 ms** | 18627 ms | 25009 ms |
> | `a7c064eb` (19.0 s, same commit as above) | **97 ms** | 18857 ms | 19000 ms |
>
> The `2306fd9` **decode is ~18.6-18.9 s in both** the 19.0 s run and
> today's 24.4 s run - identical work, identical time. The only thing
> that ever moved is the walk: **97 ms warm vs 6381 ms cold**, a 6.3 s
> swing that is the entire "regression". The feared cache confound (cell
> 9 finding cell 7's headers warm) did not materialise: the intervening
> worktree cargo build scrubbed the page cache, so both cells walked
> cold. That was luck, not design.
>
> Note the walk **never warms** at planet across `--bench N` iterations
> (all three HEAD runs ~4.78 s): each iteration reads ~30 GB and evicts
> the headers it just read. Unlike japan sparse, best-of-3 cannot escape
> the cold walk here.
>
> **New baseline: 22.8 s `--bench 3` (`39570d10`, `fb743f6`).** Retire
> 19.0 s.
>
> **Standing lesson: getparents/getid planet walls are header-cache
> dominated.** The command family was already documented as cache-state
> dominated (getid planet: 6.8 s cold / 0.2 s warm) and the *walk* has
> the same property. Never compare two getparents planet walls without
> comparing their schedule-phase disk read first. This also softens
> ADR-0006's threshold derivation, which assumed a ~45 us/blob walk cost:
> that figure is a cold-cache number and the real cost swings ~45x with
> cache state. The dispatch decision is not in question at these blob
> counts, but the threshold is softer than the ADR implies.
>
> ---
> **Superseded reasoning, kept as a negative result** (the suspicion the
> phase split killed):
> Phase shape is unchanged and the ADR-0006 dispatch still picks the
> Walker arm correctly (`getparents_schedule_blobs` 17,981,
> `getparents_blobs_skipped` 32,835 - the same 64.6 % skip rate as
> before), so this is not a dispatch misroute. The alloc profile
> (`f83c8cf1`) is clean and rules out allocator churn:
> `decompress_blob_raw` is 59.9 % of allocations, `parallel_classify_phase`
> 17.66 s of the 22.5 s wall. Suspect the decode path between `2306fd9`
> and `dcc445e`, not the dispatch. **Not a release blocker** (22 s wall)
> but it is the one real Q6 finding.
>
> **Prime suspect (code read 2026-07-14): `4776b92` "scan: make WILLNEED
> classify prefetch unconditional".** It promotes `posix_fadvise(WILLNEED)`
> over coalesced classify-schedule payload ranges to unconditional across
> all four classify consumers - including `parallel_classify_phase`, which
> IS getparents' hot phase (17.66 s of a 22.5 s wall). Its own commit
> message is the tell: the A/B that justified it was **europe-only**
> (check-refs 56.8 -> 53.3 s, tags-filter 61.8 -> 58.3 s) and planet
> safety is asserted rather than measured - *"planet is not
> over-prefetched even though the win is europe-scale."* That is the
> regime-transfer trap this file already documents twice (the ALTW
> europe-vs-planet compression axis; the zstd:1 ceiling cell).
> Corroboration: **check-refs at planet also came back +11.9 %** in the
> same suite, and check-refs is the very command whose *europe* number
> justified the commit - so the "drift" dismissal on that row above may
> be wrong. Counter-evidence: check-ids is also a classify consumer and
> came back flat (+0.4 %), so the story is not clean.
> ~~Cheapest test: `brokkr getparents --commit 4776b92` vs
> `--commit 4776b92^`.~~ **Not needed - the phase split above closed it
> first, for free.** The WILLNEED window was also read and found sound:
> it correctly slides 256 MiB ahead of the dispatched entry, so the
> "planet is over-prefetched" story had no mechanism behind it either.
> Two cells saved by reading a sidecar before queueing a bench.

**Same-day A/B rule for I/O-bound cells.** The 8k cells missed their
pre-registered absolute bounds (52.8 s + 10 %, 33.2 s + 10 %) for a
reason that had nothing to do with the code: the machine's steady
sequential buffered read rate on this file was ~2.95 GB/s on the
evening of 2026-07-10 and ~2.03 GB/s on the morning of 2026-07-11.
Both runs are flat-rate for their whole duration; page-cache eviction
(interposing a 26 GB europe read) changed nothing; no reboot between
the sessions; `read_ahead_kb` 128 both days. Re-running the *reference
commits themselves* via `--commit` reproduced today's regime, not
yesterday's - the April getid binary did 48.2 s against its own 33.2 s
from the previous evening. Verdicts for those cells were therefore
taken on same-day `--commit` A/B, which is the discipline to reuse:
absolute wall bounds carried across days on I/O-bound cells can be off
by ~45 % from environment alone.

**Two refuted shapes** hit the gates before the landing settled (both
recorded in ADR-0006): classify on the pipeline consumer thread
(getparents 8k 142.8 s, `cbd4c0a3` - one thread serialized a billion
way-ref checks behind the parallel decode) and the pipelined reader as
getid's scan arm (53.9 s, `3a9990e5` - per-frame pipeline overhead
times 1.45 M small blobs loses 62 % to sequential streaming reads).

**Estimator accuracy** (`walk_estimated_blobs` vs exact count):

| encoding | actual | estimate | error |
|---|---:|---:|---:|
| planet primary | 50,816 | 36,063 | -29 % |
| europe | 522,168 | 458,132 | -12 % |
| planet 8k | 1,453,433 | 899,866 | -38 % |

The head-of-file sample over-weights node frames, which run larger
than the file-wide mean on all three encodings, so the estimator
undershoots consistently. Every arm choice was still correct - the
dispatch discriminates a >= 3x gap and the worst error is 1.6x - but
any future move of the 150 k constant must price this bias.

### Fused command transforms (ADR-0009, 2026-07-12, plantasjen)

The FullScan / pipelined command arms above now run their transform
inside the decode workers rather than materializing 64-block batches and
re-dispatching to a second rayon pool. Same-night A/B at commit
`a65cecc`, best of `--bench 3`:

| cell | baseline | fused | delta |
|---|---:|---:|---:|
| getid planet-8k `--add-referenced` | 197.9 s | **182.7 s** | -7.68 % |
| getparents planet-8k FullScan | 63.0 s | **58.9 s** | -6.51 % |
| tags-filter `-R` planet-8k | 45.9 s | **42.7 s** | -6.97 % |
| tags-filter `-R` planet primary | 52.8 s | **49.5 s** | -6.25 % |
| getid planet primary `--add-referenced` | 96.3 s | **83.9 s** | -12.88 % |

The getid cells are `--add-referenced` pass 2 (heavier than the
include-mode getid rows above). getid primary pass-2 peak RSS also fell
1.18 GB -> 596 MB (the batch materialization is gone). The altw
decode-all europe-raw signal cell OOM-killed on this 23 GB host, so its
large-scale wall is unmeasured. Full arc and the reverted sibling
experiments (batched pipeline, byte knobs) in
`reference/performance-history.md` "Env-gated read-path batch".

### HeaderWalker / next_header_skip_blob regression check (commit `436998b`, re-measured 2026-04-26 at `16e3694`)

The `436998b` (2026-04-26) read-path truncation alignment added small
per-blob branches in `HeaderWalker::next_header` (payload-extent
check, probe-pread arm cleanup) and rewired
`BlobReader::skip_blob_body` from `seek_relative(n)` to
`seek_relative(n-1) + read_exact(1)`. First-order analysis predicted
no extra syscalls in steady state. Verified empirically against the
four heaviest users of the touched primitives:

| Command | Pre-`436998b` | Post-`436998b` (`16e3694`) | Δ |
|---|---:|---:|---:|
| getid (include) | 6.1 s (`24362e36`) | 6.8 s (`41413398`) | +0.7 s, +11 % (within `--bench 1` noise at sub-10 s walls) |
| check --refs | 72.5 s (`862547e4`) | **53.8 s** (`7d9f5dfd`) | **−18.7 s, −25.8 %** |
| add-locations-to-ways external | 603.7 s (`aa0dc719`, post-A1) | **546.0 s** (`7fd04130`) | **−57.7 s, −9.6 %** |
| build-geocode-index | 432.9 s (`b4b25c05`) | 424.8 s (`2b412af4`) | −8.1 s, −1.9 % (within noise) |

No regression from the truncation-alignment commit. check-refs and
ALTW external both moved materially faster than noise; the wins are
attributable to commits landed between the prior baselines and
`16e3694` (the ALTW row carries A1's effect against the older
pre-A1 row in the audit comment).

### check --refs: missing references in ID sets (bygg, 2026-10-27)

`23c53a0b` counts missing references into one `IdSet` per category
instead of one `i64` per missing occurrence sorted at the end. Valid
input, `check-refs --dataset planet --bench 1`, interleaved against the
parent `d353ef87`: 42.0 s (`739861b7`) and 41.9 s (`b3d8343d`) before,
41.5 s (`692de673`) and 41.6 s (`1b6bad0d`) after - neutral, as expected
with near-zero missing refs.

The case the change is for, a ways-only file where every way ref is
missing (`--snapshot ways-only`, a `cat --type way` output). These runs
exit 1, the check rightly failing; brokkr stores them since brokkr#45,
with `meta.exit_code=1`:

| Input | Commit | Wall | Peak RSS | UUID |
|---|---|---:|---:|---|
| europe ways-only | `d353ef87` (parent) | **300.9 s** | **25.2 GB** | `7cf2fa34` |
| europe ways-only | `79c14c13` | **15.9 s** | **2.0 GB** | `c8047161` |

The parent filled its vector in 25 s and then sorted it out of swap for
about 4.5 minutes on a 30.5 GiB host; the current code's peak is the node
bitset, the missing-node set over the same range and the decode buffers.
Its 15.9 s includes a cold 5.6 s schedule walk; an earlier unstored
warm-cache pair read 12.0 s against 5m28s. Denmark ways-only runs finish
in under a second, too short for the sampler to catch each phase's peak,
so they are not tabled.

### IdSet dense chunks allocated zeroed (bygg, 2026-10-27, commit `03e3792d`)

`new_dense` (the only place a dense chunk is created: insert promotion,
`promote`, union and `pre_allocate` all go through it; `clone_chunk` copies
real data and still writes every word) now uses `new_zeroed_slice`, so a
pre-allocated chunk is untouched zero pages instead of 4 MiB written up
front. Matched `--bench 1` cells against the parent `13100499`, alternating,
two pairs each, bygg, no `fstrim` (not permitted in that session):

| Cell | New `03e3792d` | Parent `13100499` | UUIDs (new, parent, new, parent) |
|---|---:|---:|---|
| check-refs planet | 41.9 s, 41.7 s | 42.1 s, 41.7 s | `c60fdbbd`, `e7f92dcc`, `e9198203`, `6365d8a4` |
| renumber planet | 143.2 s, 143.9 s | 165.5 s, 142.6 s | `28d1e4d7`, `a59a205b`, `92fa012b`, `4830e019` |
| build-geocode-index planet | 317.3 s, 318.3 s | 313.8 s, 313.2 s | `54449345`, `84c8d437`, `25966c74`, `fabf1b04` |
| check-refs malta | 101 ms | 101 ms | `66460942`, `8c78c1c2` |

All neutral. The parent's 165.5 s renumber is a single outlier (its pair
partner and the next parent cell read 142.6 s). Geocode is 1.3 % slower in
total with no phase moving more than the noise (compare `25966c74` against
`fabf1b04`). check-refs planet compare (`e9198203` vs `6365d8a4`): scan,
nodes and ways phases identical in duration, cores and peak RSS (about
2.1 GB sampled), as expected since planet writes every page anyway. Malta
sampled only a start and an end row, so no RSS comparison exists.

Release builds gain nothing: `check --refs tests/test.osm.pbf` peaks at
7012 kB on `03e3792d` and 7052 kB on the parent (zsh `time` `%M`), so the
optimizer was already lowering the parent's `collect()` of zeros to a
zeroed allocation, and the floor pre-allocation was never resident there.
The 1.2 GB seen for a three-node `check --refs` came from a debug test
build, which keeps the word-by-word write. The change makes the zero pages
explicit rather than an optimization, which is what the debug-built test
suite gets. Even a small real extract spreads its IDs over the whole ID
history and writes most pages of most chunks, so extracts do not shrink
either.

### Kind claims checked at decode (bygg, 2026-10-27, commit `7ecb8a43`)

Every block now records the element kinds it holds in its construction
validation pass, the classify pools compare them with a scheduled blob's
claimed kind under `KindClaims::Refuse` (adopted by `check --refs` and
`check --ids`), and the pipelined reader carries the setting unused.
Matched `--bench 1` cells against the parent `e8ae74f3`, alternating parent
then new, three pairs each, bygg, untrimmed:

| Cell | New `7ecb8a43` | Parent `e8ae74f3` | UUIDs (parent, new) per pair |
|---|---:|---:|---|
| check-refs planet | 41.9, 41.5, 41.5 s | 41.8, 41.2, 41.3 s | `9a095693` `efbdc9ab`, `584ca62b` `fcf76ae8`, `8bfaed50` `7cb61453` |
| check-ids planet | 38.1, 38.1, 38.2 s | 37.8, 37.9, 38.0 s | `3d84b122` `418b6270`, `13be238e` `81d53a8b`, `7e1ec65b` `519d0c43` |
| tags-filter -R planet (pipelined, `w/highway=primary`) | 30.5, 30.5, 30.4 s | 30.4, 30.4, 30.4 s | `70a58d33` `0e6b810d`, `d6694f01` `b7ae4e70`, `794d7302` `cb88fee3` |

Medians move by 0.5 % (check-refs), 0.5 % (check-ids) and 0.3 %
(tags-filter), inside the noise band. The checks' sign is consistent across
pairs; the phase splits (`584ca62b` vs `fcf76ae8`, `13be238e` vs `81d53a8b`)
put the difference in the way phase, about 0.2 s of 11 to 14 s at the same
cores, with the schedule walk and the node phase identical.

The adoption by every other per-kind phase (`431d3130`) and the raw
walkers returning the kinds they read (`43e20723`) were measured where they
add work per element, the raw-walker adopters; elsewhere the check is one
compare per decoded block. Interleaved `--bench 1` cells against
`6bb0ad09`, two pairs each, bygg, untrimmed:

| Cell | New `43e20723` | Parent `6bb0ad09` | UUIDs (new, parent, parent, new) |
|---|---:|---:|---|
| extract simple planet, Europe bbox | 119.1, 120.6 s | 118.6, 122.1 s | `7341cc2c`, `0f7da82d`, `0111601a`, `9a2799ee` |
| build-geocode-index planet | 316.7, 315.2 s | 321.1, 321.2 s | `02bd5b17`, `c6f71f3e`, `838241cf`, `471766ed` |

Both inside the noise band (extract medians 119.9 against 120.4 s, geocode
316.0 against 321.2 s).

## Renumber (external mode)

Planet-scale renumber via IdSetDense rank-based O(1) lookup (replaces
the original 256-bucket radix partition). Wire-format splice rewriters
for all three element types - pass 1 (DenseNodes), stage 2d (ways),
and R2d (relations) - patch only the ID/ref fields and copy everything
else verbatim as raw bytes. No BlockBuilder, no PrimitiveBlock
construction. Pass 1: 4 work-stealing workers. Stage 2d: 6 workers.
R2d: parallel with inline rank() dispatch (relation_map replaced by
`relation_id_set.rank()`). All member-ref lookups via
`node_id_set.rank()` + `way_id_set.rank()` inline - no flat temp
files. Zero scratch disk usage. Single shared input fd across all
data phases (the header read and the schedule walk have their own).
Atomic index dispatch (no `Arc<Mutex<Receiver>>`). Output
defaults to zlib:1. The arena cap requested at the start of `renumber_external()` through
the caller's process actions (the CLI applies it) prevents glibc cross-thread arena
fragmentation.

### Planet (87.7 GB indexed, 11.6B elements, plantasjen)

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways /
14,124,889 relations / 12,435,459,911 way refs.

### Memory

Peak anon 3.3 GB (commit `cb99106`). Single shared `IdSetDense` with
`AtomicU8::fetch_or` for concurrent pass 1 writes (~1.5 GB node bitset
+ rank index, ~200 MB way bitset + rank, ~20 MB relation bitset).
Zero temp disk. The arena cap requested at the start of `renumber_external()` through the
caller's process actions (the CLI applies it) caps glibc arena growth from cross-thread OwnedBlock `Vec<u8>` frees.

### Phase breakdown (commit `cb99106`, planet, `--bench 1`, UUID `f9098cab`)

| Phase | Duration | Peak Anon | Share |
|---|---:|---:|---:|
| Schedule scan | **16.6 s** | - | 9% |
| PASS1 (4 workers, wire-format nodes) | **95.3 s** | 2.1 GB | 49% |
| STAGE2D (6 workers, fused way resolve + wire-format ways) | **76.8 s** | 3.3 GB | 40% |
| R1 (sequential wire-format relation ID scan) | **3.2 s** | - | 2% |
| R2D (parallel wire-format relations, inline rank()) | **1.9 s** | - | 1% |
| **TOTAL** | **194 s (3m14s)** | **3.3 GB** | - |

## Extract

Plantasjen. Best of 3 runs (or single-sample where noted), indexed PBFs.

| Dataset | Size | simple | complete | smart | Commit |
|---------|------|--------|----------|-------|--------|
| Denmark | 487 MB | 2259 ms | 2399 ms | 2693 ms | `aacbe80` |
| Japan | 2.4 GB | **3.8s** | **3.7s** | **4.7s** | `cadc3e6` |
| Europe | 32.4 GB | **96.3s** | **164.9s** | **181.4s** | `cadc3e6` |
| Planet † | 87.7 GB | - | - | **279s** | `cadc3e6` |

† Planet smart extract: single-sample `--bench 1`, Europe bbox, UUID
`2d028196`. Peak anon RSS 11.17 GB on 32 GB host (27.9 GB avail at run
start, 16.7 GB headroom to the ~25 GB "ship as-is" threshold). Peak
anon is dominated by PASS3 write work (bbox-sized), not by PASS1
scanning the input file. The mechanism identified during the
2026-04-10/11 investigation was a cold-arena-page residency cascade
triggered by post-PASS1 header scans touching pages glibc had reserved
but not populated; fixed by plumbing the PASS1 schedule forward into
PASS2 and PASS3 via `Pass1Result::pass3_blob_schedule` and
`pread_write_pass_with_schedule`.

Denmark bbox `12.4,55.6,12.7,55.8`, Japan bbox `139.5,35.5,140.0,36.0`,
Europe and Planet bbox `-25.0,34.0,45.0,72.0` (full-continent).

Simple extract uses a 3-phase barrier pipeline with parallel classification
and raw frame passthrough. Each phase (nodes, ways, relations) classifies
blobs in parallel then writes matching raw frames via pread workers - no
decode+re-encode.

Complete-ways and smart pass 1 (`collect_pass1_generic`) uses three-phase
parallel pread classification (nodes → ways → relations) via a reusable
`parallel_classify_phase` helper. Smart pass 2 (way dependency resolution)
also uses `parallel_classify_phase`, replacing the old sequential BlobReader
scan. Workers pread + decompress + classify in parallel, sending compact
results back to the consumer. Write passes use pread-from-workers with full
PrimitiveBlock lifecycle per worker.

## Multi-extract

Single-pass simple strategy on sorted input: read PBF once, classify each
element against N regions, write to N sync-mode PbfWriters
(sync-mode writers over the staged files; publication syncs each output
after the final flush, once `MULTI_REL_WRITE_END` and every other phase
marker has ended, between `PUBLISH_START` and `PUBLISH_END`; the `writer_sync_all_ns` and
`writer_sync_all_calls` counters attribute that cost). The rows below
predate the per-output sync and the 256 KiB per-output buffer. 3-phase
barrier (nodes → ways → relations) with per-region `IdSetDense` +
`BlockBuilder`. 5 disjoint longitude strips per configured bench.

| Dataset | 5-region wall | Commit | UUID | Mode |
|---------|--------------:|--------|------|------|
| Japan   | **7.7 s**  | `b7cd0e4` | `08fefe51` | `--bench 3` |
| Europe  | **799.9 s** | `b7cd0e4` | `c1ff6ec9` | `--bench 1` |
| Planet  | 965 s   | `7e9c2e9` | `1cd62e90` | `--bench` (pre-instrumentation) |

### Europe phase breakdown (commit `b7cd0e4`, UUID `c1ff6ec9`, plantasjen, `--bench 1`)

| Phase | Wall | % of total |
|---|---:|---:|
| MULTI_SCHEDULE_SCAN (header walk, 3 schedules + `NodeBlobInfo`) | 26.0 s | 3.3 % |
| MULTI_NODE_CLASSIFY | 15.8 s | 2.0 % |
| **MULTI_NODE_WRITE** | **413.4 s** | **51.7 %** |
| MULTI_WAY_CLASSIFY | 13.7 s | 1.7 % |
| **MULTI_WAY_WRITE** | **317.5 s** | **39.7 %** |
| MULTI_REL_CLASSIFY | 0.9 s | 0.1 % |
| MULTI_REL_WRITE | 12.1 s | 1.5 % |
| **MULTI_EXTRACT total** | **799.4 s** | 100 % |

Write phases dominate Europe: `NODE_WRITE` (52 %) + `WAY_WRITE` (40 %) =
92 % of wall. These are the real optimization targets.

Counters emitted (UUID `c1ff6ec9` values):
- `multi_extract_region_count=5`
- `multi_extract_node_blobs`, `multi_extract_way_blobs`,
  `multi_extract_relation_blobs` (schedule sizes)
- `multi_extract_nodes_written`, `multi_extract_ways_written`,
  `multi_extract_relations_written` (cross-region totals)

## Tags-filter

Two-pass architecture: pass 1 classifies blobs in parallel (parallel
classification + lightweight scanner), closure + way dep scans also
parallelized via `parallel_classify_phase`, pass 2 writes matching
elements.

### Planet axes (commit `16e3694`, plantasjen, `--bench 1`, `w/highway=primary`)

| Axis | Wall | UUID | Notes |
|---|---:|---|---|
| default (transitive two-pass) | **108.2 s** | `7e4301f9` | reproducible against the post-`01c67da` 119.9 s row |
| `-R` (single-pass, keep referenced) | 51.8 s | `cf116a6b` | flat vs `f262f068` baseline |
| `--invert-match` | 461.2 s (7m41s) | `6665605a` | first stored measurement; ~1 % of ways match `highway=primary`, so invert touches ~99 % of ways |
| `--remove-tags` (`-t`) | 111.8 s | `44d96d0a` | first stored measurement; two-pass with tag-stripping in pass 2 |
| `--input-kind osc` (osc 4913, 1-OSC) | 6.2 s | `37f360d2` | OSC parse + filter only; no PBF read |

`--invert-match` is the headline outlier: 4.3× the match path. The
asymmetry is workload-shape: keeping primary highways drops ~99 % of
ways (and most of their referenced nodes) so the writer's pass-2
output is small; inverting keeps ~99 % and the writer becomes the
ceiling.

### Planet `-j N` sweep (default two-pass, `w/highway=primary`, commit `16e3694`)

`-j` only affects the two-pass parallel path; the single-pass `-R`
path ignores it (CLI rejects the combination).

| `-j` | Wall | UUID |
|---:|---:|---|
| 4 | 184.0 s | `46d83578` |
| 8 | 123.3 s | `2a8fe06e` |
| 16 | 112.2 s | `b1d0c53d` |
| 24 | 111.1 s | `cffa644a` |

Saturates at `j16`; `j24` is within single-shot noise. Default
auto-detect on this 24-thread host lands the same workload at
108.2 s (`7e4301f9`), inside the noise band.

## Merge-changes

`pbfhogg merge-changes` squashes N OSC (gzip + XML) inputs into one OSC
output. Two production code paths: **streaming** (`write_streaming`,
the default) and **simplify** (`--simplify`, builds an in-memory
overlay then writes the latest change per object via a `BTreeMap`
dedupe).

The 1-OSC vs N-OSC axis is the load-bearing distinction: a single OSC
measures fixed setup + per-OSC parse + per-OSC write; an N-OSC squash
measures the N inputs' work plus a merge/write tail. **The parallel
shape only fires when N > 1**, and the 1-OSC fast path keeps the
original serial streaming pipeline (parse + emit + gzip interleaved on
one thread) so single-OSC walls are unchanged.

### Streaming-path planet 7-OSC: 5.0× speedup (commit `99057fa`, plantasjen)

| Stage | UUID | Wall | vs serial baseline |
|---|---|---:|---:|
| Serial baseline | `c612c5e6` (commit `fb1719c`) | 272.6 s | 1.00× |
| Parallel parse, serial drain | `07ee92ee` (commit `43dd620`) | 235.8 s | 1.16× |
| **Parallel-drain (current)** | **`b6e964cc`** | **54.7 s** | **5.0×** |
| 1-OSC fast path | `941a5784` | 44.2 s | (within noise of pre-parallel 43.1 s) |

The middle row is the abandoned intermediate shape. It correctly
parallelized the 12.6 s parse phase (21× phase speedup) but exposed a
223 s serial drain on the main thread - per-change `quick_xml::Writer`
emit + zlib level-1 gzip-compress of 26.3 M changes, one thread,
~118 K changes/s ceiling. The current shape moves emit + gzip onto the
worker threads: each worker runs the full per-input pipeline (parse +
re-emit + gzip-compress) into its own `OscWriter<Vec<u8>>` and returns
self-contained gzip bytes. Main thread writes a pre-built prelude
gzip member, the 7 worker chunks in input order, and a postlude gzip
member. Multi-member gzip is valid (osmium, osmosis, gzip CLI,
`MultiGzDecoder` all support it); output decompresses to the
concatenation of all members.

Since the measurement below, emission runs through a bounded ordered
window instead of collecting every member first: input `k` is admitted
only once `k` is less than the number of members already written plus
the pool's thread count, and each member is written as soon as it and
its predecessors are done. At most one member per pool thread is
alive, so memory no longer grows with the number of inputs (one huge
input is still held whole). Output bytes are identical. When the input
count is at most the thread count, as in the 7-OSC planet cell on a
24-thread host, every input is admitted at once and the shape matches
the run below except that member writes overlap the emit phase:
`MERGECHANGES_PARALLEL_EMIT` now spans the per-member writes, and
`MERGECHANGES_DRAIN` covers only the postlude and flush. Not yet
re-benched.

Phase breakdown of the 54.7 s `b6e964cc` run:

- `MERGECHANGES_PARALLEL_EMIT`: **54.1 s** - 7 workers running parse +
  XML emit + gzip-compress in parallel. Worker completion order
  (from per-worker `merge_changes_decompress_ns` counter timestamps):
  31.2 s, 33.3 s, 36.5 s, 40.3 s, 41.4 s, 47.9 s, 54.1 s. The longest
  worker is OSC 3 (5.8 M changes, the heaviest input by a factor of
  ~2 over the others) - the wall is gated by the heaviest single OSC.
- `MERGECHANGES_DRAIN`: **0.59 s** - main thread concatenates prelude
  + 7 worker chunks + postlude through a `BufWriter<File>` raw-bytes
  copy. Drain is essentially free; the work it used to do is now
  spread across the workers.

### Cross-dataset matrix (commit `16e3694`, plantasjen, `--bench 1`)

Pre-parallel baselines, retained for cross-dataset shape:

| Dataset | OSC count | Default wall | `--simplify` wall | Per-OSC effective rate | UUIDs (default / simplify) |
|---|---:|---:|---:|---:|---|
| Germany | 1 (`--osc-seq 4705`) | **2.5 s** | - | 2.5 s | `1ba15f41` / - |
| Germany | 7 (`--osc-range 4706..4712`) | **18.0 s** | 16.2 s | 2.6 s/OSC | `91cb8465` / `638a4b99` |
| Europe | 7 (`--osc-range 4716..4722`) | **153.2 s** | 152.9 s | 21.9 s/OSC | `993ae62a` / `745ee521` |
| Planet | 1 (`--osc-seq 4913`) | **43.1 s** | - | 43.1 s | `76f78e8b` / - |
| Planet | 7 (`--osc-range 4914..4920`) | **267.2 s (4m27s)** | 262.2 s (4m22s) | 38.2 s/OSC | `bef0f1fa` / `c0d140b6` |

Planet 7-OSC walls have dropped substantially at `abd1d9e`: default
to **54.7 s** (UUID `b6e964cc` at `99057fa`), `--simplify` to
**73.7 s** (UUID `3e3ef119` at `abd1d9e`). The germany/europe rows
have not been re-benched; expected speedup at those scales is similar
in shape (max-per-OSC + small drain) but smaller in magnitude because
the heaviest OSC dominates and germany 7-OSC walls are already short.

### Simplify-path planet 7-OSC: 3.6× speedup (commit `abd1d9e`, plantasjen)

| Stage | UUID | Wall | vs serial baseline |
|---|---|---:|---:|
| Pre-parallel baseline | `c0d140b6` (commit `16e3694`) | 262.2 s | 1.00× |
| Parallel parse only | `37fbe5b5` (commit `488d1f0`) | 220.9 s | 1.19× |
| **Parallel parse + parallel write_simplified** | **`3e3ef119`** | **73.7 s** | **3.6×** |

Phase breakdown of the 73.7 s `3e3ef119` run:

- `MERGECHANGES_PARALLEL_PARSE`: **12.3 s** - same shape as the
  streaming-path parse (workers each call `parse_osc_into` into a
  local `ChangeStream`, main thread concatenates in input order).
- `MERGECHANGES_SIMPLIFY`: **6.9 s** - serial `BTreeMap` dedupe of
  26.3 M changes into 25.4 M deduped; cheap relative to parse + emit.
- `MERGECHANGES_PARALLEL_EMIT`: **49.4 s** - the new shape. After
  dedupe, each non-empty action group (creates / modifies / deletes)
  is split into chunks of size `group.len().div_ceil(num_workers)`
  via rayon's `par_chunks`. Each worker emits its chunk as a
  self-contained `<action>...</action>` gzip member and returns the
  bytes. Same multi-member gzip output shape as the streaming path.
  This phase replaces a previous ~197 s serial XML + gzip emit
  through a single OscWriter (4.0× phase speedup).
- `MERGECHANGES_DRAIN`: **0.33 s** - main thread concatenates prelude
  + per-chunk members in (group, chunk-index) order + postlude.

The simplify wall is now 19 s slower than the streaming wall on the
same input. The gap is the dedupe phase (6.9 s) plus a slightly less
efficient parse fan-out (workers parse to a separate ChangeStream
buffer in simplify, vs the streaming path's fused parse + emit
through `parse_osc_streaming`).

Pre-parallel observations from the matrix that still hold:

- **`--simplify` is a near-zero overhead** at every scale measured
  (planet −5.0 s / −1.9 %; europe −0.3 s; germany −1.8 s / −10 %
  inside single-shot variance). The `BTreeMap` dedupe is cheap
  relative to the parse cost; the simplify path's win comes when
  multiple OSCs touch the same object IDs (which dailies on planet
  do at low percentages).
- **Per-OSC rate scales with input size, not OSC count**. Germany
  2.6 s/OSC, europe 21.9 s/OSC, planet 38.2 s/OSC. Each OSC is
  roughly proportional to the dataset's daily-diff size; planet's
  ~140 MB/OSC takes ~38 s of gzip + XML parse on the main thread.

### Pre-flight measurements that locked in the implementation choice

The `c612c5e6` instrumented re-bench (commit `fb1719c`) added a
`TimedRead<R>` wrapper around the file reader to attribute gzip
decompress wall separately from the surrounding `quick_xml` machinery:
**gzip = 1.5 % of wall** (3.98 s of 272.6 s, 1.4-1.5 % per-OSC). That
killed the parallel-decompress + sequential-XML alternative outright -
no win to extract there - and forced the buffer-and-drain shape that
landed at `43dd620` and `99057fa`. The same pre-flight added
`merge_changes_changes_per_osc` (per-input change-count delta) which
sized peak per-worker `ChangeStream` residual at 5.8 M changes for
the heaviest OSC.

The `4fc8e35` `--hotpath` companion (UUID `ee108ec9`, 2026-04-27)
recorded 7 calls to `parse_osc_streaming` totalling 264.42 s = 100 %
of wall, with per-OSC avg 37.8 s and **P95 50.9 s**. The `--alloc`
companion (UUID `13615a4a`) attributed 62.4 GB cumulative
allocation across the 7 calls entirely to `parse_osc_streaming`;
no other function showed on the alloc table. The parser was the only
allocation hotspot pre-parallel.

## Pipeline end-to-end

Bootstrap (one-time): `cat` → `add-locations-to-ways` → enriched PBF.
Steady state: `apply-changes --locations-on-ways` (daily diffs).

### Planet bootstrap (plantasjen, commit `3d977a0`)

| Step | Time | Output |
|------|------|--------|
| cat (indexdata generation) | 497s (8m) | 87.7 GB |
| add-locations-to-ways (external) | 953s (15.9m) | 88.4 GB |
| **Total bootstrap** | **~24m** | - |

### Europe bootstrap (plantasjen, commit `3d977a0`)

| Step | Time | Output |
|------|------|--------|
| cat (indexdata, `--type` filtered) | 112s | 33.6 GB |
| add-locations-to-ways (external) | 400s (6.7m) | - |
| **Total bootstrap** | **~8.5m** | - |

## build-geocode-index

Reverse geocoding index build. 4-pass pipeline: nodes (address points + dense
node index), ways (streets, buildings, interpolation), relations (admin boundary
assembly + simplification), S2 cell assignment (fine level 17 + coarse level 14).

| Dataset | PBF size | Time | Index size | Addr points | Streets | Admin | Commit |
|---------|----------|------|------------|-------------|---------|-------|--------|
| Denmark | 465 MB | **7.1s** | 172 MB | 2.6M | 314K | 2K | `f42da6e` |
| Japan | 2.4 GB | **26.7s** | - | - | - | - | `c33e8cc` |
| Germany | 4.5 GB | **1813s** (30m) | ~1.8 GB | 19.8M | 3.3M | 43K | `ed34092` |

### Japan sidecar profile (commit `5776b67`, plantasjen, --bench --sidecar)

| Phase | Duration | Peak RSS | Peak Anon | Disk Read | Disk Write | Majflt |
|-------|----------|----------|-----------|-----------|------------|--------|
| Pass 1 (relations) | 0.9s | 9 MB | 5 MB | 2.3 GB | 0 | 0 |
| Pass 2 (nodes+ways) | 55-60s | **19 GB** | **325 MB** | - | - | 1.3M (plateau, no thrash) |
| Pass 3 (S2 cells) | 1.9s | 352 MB | 273 MB | - | 539 MB | - |

Sequential reader (commit `5776b67`) keeps anon bounded at 325 MB - no
PrimitiveBlock cross-thread retention. The 19 GB peak RSS is the DenseMmapIndex
mmap (file-backed, fits in RAM at Japan scale). At Europe/planet scale this
mmap would thrash (same as dense ALTW).

Denmark: 0 interpolation ways (Scandinavian precise addressing). Germany: 78
interpolation ways with `addr:interpolation` + `addr:street`, 71/78 resolved.

### Comparison with traccar-geocoder

No directly comparable data - different hardware, different format, different
build architecture (traccar uses C++ with libosmium, single-threaded, all data
in RAM). Numbers from the HN thread (2026-03-21):

| Dataset | traccar-geocoder | pbfhogg | Notes |
|---------|-----------------|---------|-------|
| Australia/Oceania (~1.1 GB) | ~15 min (KomoD) | - | Not tested |
| Germany (4.5 GB) | - | **30.9 s** | After 2026-04-18 optimization arc |
| Planet (~87 GB) | 8-10 hours (192 GB RAM) | **7m12s** (27 GB host) | After 2026-04-18 optimization arc |

Planet (validated, commit `82db8ed`, UUID `b4b25c05`, 2026-04-18,
plantasjen, `--bench 1`): **432.9 s (7m12s), ~25 GB peak anon RSS** in
`GEOCODE_PASS3_STAGEB_FINE`.

Our index is larger due to segment-level indexing (6 bytes vs 4 per
entry), dual fine+coarse cell indices, and u64 node offsets. All
intermediate data is still held in RAM during the build - planet fits
comfortably on a 27 GB host after this arc, though a streaming-temp-file
refactor would be needed for smaller hosts.

### Planet phase breakdown (`82db8ed`, UUID `b4b25c05`)

| Phase | Wall | Avg cores | Peak anon |
|---|---:|---:|---:|
| Pass 1 (relations) | 36.6 s | 0.5 | 1.25 GB |
| Schedules (one-walk) | 16.5 s | 0.2 | 1.31 GB |
| Pass 1.5 scan | 17.6 s | **20.3** | 3.03 GB |
| Pass 2a parallel nodes | 66.5 s | 13.2 | 12.16 GB |
| Pass 2b parallel ways | 124.4 s | 4.9 | 13.99 GB |
| Pass 2 admin assembly | 10.0 s | **21.9** | 6.05 GB |
| Pass 2 interp resolve (sequential) | 30.6 s | 1.0 | 9.04 GB |
| Pass 3 admin cells | 4.8 s | 17.4 | 6.00 GB |
| Pass 3 fine Stage A | ~50 s | 5.4 | 5.97 GB |
| Pass 3 fine Stage B | 39.4 s | 3.5 | **22.53 GB** (governing peak) |
| Pass 3 coarse Stage B | 13.0 s | 5.3 | 14.57 GB |
| Misc (flush/mmap/write/admin index/cleanup) | ~22 s | - | - |

Pass 2b dominates at 124 s / 29 % of wall. Pass 2 interp resolve was the
only sequential-by-design phase left at this commit (30.6 s / 7 %); it
was replaced with a parallel sorted-CSR build plus parallel endpoint
resolution - see "Interpolation endpoint CSR (interp resolve
parallelized)" below.

### Interpolation endpoint CSR (interp resolve parallelized)

`resolve_interpolation_endpoints_mmap` (`src/geocode_index/builder/interp.rs`)
replaced its transient `FxHashMap<u64, Vec<u32>>` spatial index with a
sorted CSR (compressed-sparse-row) built in parallel, and parallelized
endpoint resolution over interpolation ways. Single-run, dirty-tree
validation (host plantasjen, base commit `7d5f6c1`, `--bench 1 --force`
phase spans from `brokkr sidecar` durations - honest caveat: not a
`--bench 3` verdict and not stored in `.brokkr/results.db`):

| Dataset | Phase | Before (sequential) | After (parallel CSR) | Speedup |
|---|---|---:|---:|---:|
| Planet | `GEOCODE_PASS2_INTERP_RESOLVE` total | 30.6 s | 2.76 s | ~11x |
| Planet | INDEX/CSR-build | (included above) | 2.64 s | - |
| Planet | ENDPOINTS | (included above) | 0.092 s | - |
| Europe | interp-resolve total | ~15 s (INDEX ~12 s + ENDPOINTS ~3 s) | 1.29 s | ~11.6x |
| Europe | INDEX/CSR-build | ~12 s | 1.25 s | - |
| Europe | ENDPOINTS | ~3 s | 0.018 s | - |

The win is the CSR data-structure replacement (the parallel sorted
build), not the endpoint parallelism - endpoint resolution is only
92 ms at planet, negligible against the 2.64 s CSR build. A prior
naive attempt (fold/reduce hashmap merge, commit `363c579`, reverted)
regressed Europe's index build to 23.7 s; the sort-based CSR avoids
that failure mode because its merge step is a linear group-by over
already-sorted pairs, not a hashmap union.

Correctness: Germany build reported 71/78 interpolation ways resolved,
identical to the pre-change baseline.

traccar's index is more compact (18 GB planet) because it uses f32 coords,
u8 node counts, u32 offsets everywhere, whole-way indexing (4 bytes/entry),
and no coarse fallback. Our format trades size for query precision (segment-
level reads, i32 coords, wider offsets) and rural coverage (coarse index).

Query latency not yet benchmarked. Both architectures use the same algorithm
(S2 cell neighborhood + binary search + distance scoring on mmap'd data), so
sub-millisecond latency is expected.

## Diff between independent snapshots

The pbfhogg CLI command is `diff` (single command). Its performance
shape depends on the inputs: two PBFs with byte-level blob overlap
(e.g. one derived from the other via `apply-changes`) trigger a
byte-equal fast-path that skips decode for unchanged blobs; two
independent snapshots (e.g. Geofabrik planets weeks apart) have no
byte-level overlap and require full decode on both sides. This
section captures the full-decode scenario.

Brokkr distinguishes the two input shapes via `brokkr diff` (which
applies an OSC to the base first so the fast-path engages) vs
`brokkr diff-snapshots` (which feeds two independent PBFs). Same
pbfhogg CLI underneath; different measurement.

### Planet (87 GB input, 93 GB output snapshot, 47-day apart)

`from=base` is the 2026-02-23 planet; `to=20260411` is the
corresponding April-11 snapshot registered under the planet dataset's
snapshot key.

Shard-based block-pair merge (`-j/--jobs N`, commit `06628d8`
2026-04-20, `--bench 1`; sharding was opt-in via explicit `-j N` at
measurement time, and became the default with `-j` defaulting to `0`
in 0.5.0 - see CHANGELOG.md). Both text and `--format osc` paths
parallelise over the same ID-range shard plan.

| UUID | Args | Wall | Peak anon RSS | vs sequential |
|---|---|---:|---:|---:|
| `22a5eb55` | `-j 16` (text, temp-file shape) | **227.5 s (3m48s)** | 586 MB | **9.5×** |
| `cdcaa4f1` | `-j 16 --format osc` (commit `16e3694`, 2026-04-26) | **293.8 s (4m54s)** | n/a | **7.6×** |

The "vs sequential" ratios divide the wall by the old single-threaded
streaming engine's planet wall, which the tree no longer contains (one
block-pair engine now serves every shard count, so the ratios cannot be
re-measured at a later commit): 2134 s for text (recorded at commit
`a9d430f2`) and 2225.6 s for `--format osc` (the same engine's recorded
planet run; its commit is not named in the tree). Dividing these by the
walls above gives 9.4 and 7.6, so the text ratio's printed 9.5 is
approximate.

Both paths stream shard output to per-shard scratch temp files
(`BufWriter<File>`) and concatenate in shard order; peak anon is just
in-flight decoded blocks. OSC's lower speedup is the serial
`assemble_osc` gzip + concat of ~45 GB of XML fragments (32.8 s,
~10 % of wall).

Phase split (planet, both paths): NODE ~73 %, WAY ~26 %, REL rounding
error. Walker phase ~15 s (pread-only via `HeaderWalker` with
`posix_fadvise(RANDOM)`; disk read 45 GB → 2.6 GB). Shard balance on
germany within 1.03× max/min. Avg cores on planet text `-j 16`: NODE
14.7, WAY 12.9, REL 14.5 out of 16 (86-92 %).

## `--direct-io` impact summary

| Workload | Bottleneck | `--direct-io` effect |
|----------|------------|---------------------|
| Merge (NA, 18.8 GB) | I/O (concurrent read+write) | **-20%** (uring+none) |
| Merge (Germany, 4.5 GB) | Mixed | Neutral |
| Cat passthrough (planet) | Sequential I/O | +5% slower |
| ALTW (europe) | Memory latency (mmap faults) | +2% slower |

`--direct-io` only helps when page cache is a bottleneck (concurrent I/O on
files exceeding available RAM). Sequential reads and memory-bound workloads
are better served by buffered I/O with kernel readahead.
