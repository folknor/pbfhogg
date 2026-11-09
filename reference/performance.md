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
- **A row's `command` is the brokkr subcommand that ran, not the pbfhogg
  one; `cli_args` is.** The 20 `inspect` hotpath rows of 2026-02-28 to
  2026-03-07 are tags counting (brokkr's `inspect` drove `pbfhogg
  tags-count`, later `inspect tags`), and the 2026-03-01 extended-metadata
  scan `e4f4508c` is stored under `fileinfo`, the subcommand `inspect`
  replaced. Select by argv (`--grep extended`, `--grep tags`), not by
  `--command`. The first hotpath profile of the full-decode inspect
  (`inspect --extended`) postdates them: from the change that measures
  `Blob::decompress_pooled` and the projection scan `scan_block_buffer`
  per blob, it splits decompression from validation plus projection, where
  before both folded into `run_pipeline` (denmark, `a9eb4708` plus that
  change, bygg, unstored: 7,396 blobs, `scan_block_buffer` 1.82 core-s,
  `decompress_pooled` 1.54 core-s, in a 0.19 s wall). `decompress_pooled`
  serves every pipelined block decode and includes the pool checkout and
  size checks, so its share is an upper bound on codec cost.
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
- **`s4_readiness_wait_ns` (2026-10-28) is not `s4_readiness_wait_ms` in
  other units.** The new counter is the add-locations-to-ways external
  stage 3 to 4 handoff as `brokkr sidecar --stalls` sees it: time from a
  stage 4 worker's first park on the router to its return, summed across
  workers, error-ending waits included, calls that never park excluded. The
  `_ms` trio is unchanged and still times every successful way-blob call,
  fast path included; `s4_readiness_wait_blobs` therefore counts way blobs,
  not blocked ones. Neither total bounds the other, so older rows, which
  have only `_ms`, do not compare with the new counter.
- **Four late-October 2026 changes affect how these counters read.**
  `merge_streaming_blobs_forced_rewrite` also counts, from `f703b02b`, the
  otherwise untouched unindexed blobs an `apply-changes --force` run must
  re-encode because no single index describes them (indexable ones are
  reframed and not counted); earlier forced runs copied those raw, and runs on indexed input
  read the same either side. `cat_blobs_decoded` and cat's "Decoded N blobs"
  count input blobs decoded, not output blobs written, from `8da26b23`; a
  `cat --clean` or `cat --type` row before it is not comparable on that
  counter. The unindexed diff and derive-changes merge decodes through a
  recycling pool from `126cbce2` and feeds the
  `pipeline_decompress_pool_*` drop counters, which earlier such rows lack;
  indexed runs at any `-j` decode into a fresh buffer per block and are
  unaffected. `publish_sync_calls` and
  `publish_sync_ns` count the publication's own syncs from `0fe60da5`,
  rather than a delta of the run's `writer_sync_all_*` totals; a CLI run
  never publishes concurrently, so the two definitions give the same values
  there.
- **Counter names across the counter-machinery commits (bygg, 2026-11-09,
  denmark, `--bench 1`, stored; `scripts/counter_names_diff.py`).**
  add-locations-to-ways `--index-type external`, `9dcf53a7^` (`8aa10c65`,
  cell `5dc8b38d`) against `a12fde41` (`91a4670b`): 193 names shared, only
  the retired `writer_write_ns` gone, 22 added (`header_walk_*`,
  `ordered_pool_*_wait_ns`, `s2_slot_fadvise_*`, `s2_slot_flush_lock_wait_ns`,
  `s4_readiness_wait_ns`, the writer and `pipeline_read_buf_*` additions);
  markers 30 shared, `CLI_COMMAND_START/END` added. Across `13dd02aa`:
  apply-changes (`2d467fbc` / `4cc4b16e`) 89 counters and 12 markers,
  apply-changes `-l` on `altw-keep-untagged` (`23477ae3` / `7a4ba6cf`) 94
  and 16, renumber (`0f5d47bd` / `8dfc6d0a`) 81 and 12, all identical. The
  verdict is bounded to these executions: names a path did not reach, and
  units or recording points under unchanged names, are not covered.
  Renumber's names changed earlier, in `fa69b667` (`_ms` timers to `_ns`,
  `*_send_ms` and `r1_scan_ms` gone, `*_window_wait_ns` and `*_reframe_ns`
  added): denmark cells before it (`6a8e9463`) do not compare with later
  ones on those counters.

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
decoded once, output in input order), so these figures describe the per-kind
one; the one-pass shape is measured in "cat --clean's peak against the README
row".

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
Planet (87 GB, daily diff): 86% rewrite. Peak memory is traced through
the dated sections below, most recent last ("apply-changes' memory step at
the node-to-way transition").

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

### `-j 1` against the automatic count (commit `eda3b670`, bygg)

What an explicit `-j 1` costs. The automatic count on bygg is 30
workers (`merge_worker_count`). `-j` sizes only the worker pool, the `-l`
coordinate slots and whether drain create runs use the executor (at
`-j 1` they are encoded on the drain thread); the writer pool and the OSC
parse are not sized by it. `--bench 1` cells, default first; peak anon
from `brokkr sidecar <uuid> --stat anon` (100 ms samples).

| Input | Default wall / peak anon | `-j 1` wall / peak anon | Ratio | UUIDs (default, `-j 1`) |
|---|---|---|---:|---|
| germany plain, OSC 4705, zlib (A B B A) | 3.09 / 3.06 s, 452 / 530 MB | 43.3 / 43.1 s, 52 / 49 MB | 14x | `ac887281` `08c6a51b`, `b784cb5a` `28c4596f` |
| planet plain, OSC 4913, zstd:1 | 94.3 s, 1.90 GB | 1471 s, 474 MB | 15.6x | `87e8b13c`, `e19c644a` |
| planet `-l` altw-keep-untagged, OSC 4913, zlib | 262 s, 2.82 GB | 5263 s, 824 MB | 20x | `41ba96ca`, `01fc3d66` |

Every phase runs at 1.0-1.3 cores under `-j 1` against 23-30 at the
default (`brokkr sidecar --compare`), so the wall is the one worker's
decode, rewrite and compression; the create segments are not significant
on either arm. On planet plain the `-j 1` peak is already reached before
the pipeline starts (the OSC parse and diff index, 474 MB in the
`CLI_COMMAND_START` segment on both arms), so what fewer workers save is
the concurrent workers' buffers and queued results, not the floor. No
code change followed: one sequential worker is what `-j 1` promises.

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
`loc_missing` 0. The refresh probe was 100 MB
(`merge_loc_refresh_probe_bytes`), about the peak-anon difference.

**The probe without its hash map (bygg, 2026-11-02).** That 100,139,008
bytes was a 91.75 MB `FxHashMap` of the 3,587,829 node upserts and deletes
(3,670,016 buckets of 25 bytes) behind an 8.39 MB filter, a copy of IDs
`SortedDiff` already holds sorted. The probe is now the filter alone over
the diff's node lists: an ID the filter admits is found by binary search in
the upserts, then the deletes, and an upsert's coordinates are read from its
record. The filter keeps its size (16 bits per key, rounded to a power of
two) and sets four bits per key in its one word instead of two, which takes
the modelled false-positive rate from about 1.3 % to about 0.35 % at this
load, so about 44 M rather than 160 M of the about 12.4 B way-ref probes
reach an exact search. `merge_loc_refresh_probe_bytes` is 8,388,608.

Planet `altw-keep-untagged` (seq 4912) + OSC 4913, `--locations-on-ways`,
interleaved `--bench 1` after `sudo fstrim -av`, both arms main-tree
builds, base `b7956e2c` and the variant that commit plus the change, swapped
by checkout. Rewrite is `merge_streaming_rewrite_ns` in core-seconds; setup
anon is the peak through `CLI_COMMAND_START`, which ends after the probe is
built.

| cell | wall | peak anon | setup anon | rewrite | UUID |
|---|---:|---:|---:|---:|---|
| variant | 260.0 s | 3,299 MB | 625 MB | 1,273 | unstored |
| base | 259.8 s | 3,324 MB | 727 MB | 1,252 | `72329201` |
| base | 259.4 s | 3,357 MB | 754 MB | 1,262 | `213ac489` |
| variant | 260.8 s | 3,213 MB | 687 MB | 1,267 | unstored |

Walls agree within 0.5 %, and the way phase (the segment ending at
`MERGE_SCANNER_BARRIER_WAIT_END`) is 113.0 s and 113.6 s against 112.9 s and
112.8 s; rewrite core-seconds are about 1 % higher, the searches the map's
lookups became. Setup anon falls by about 85 MB, the map less the run-to-run
spread; peak anon stays inside its run-to-run spread. Refresh
counters are identical in all four cells (244,817 refs in 58,701 ways, 505
way blobs searched, 163 rewritten only for the refresh).

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
planet did not fit brokkr's stdout capture when these cells ran, so germany with OSC
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
| inspect --tags `-j 16` | `--bench 1` | **168.3 s** | `00e00846` (`de8daf1`) | serial-merge era; superseded on bygg by "inspect tags counts into a sharded table" below |
| inspect --tags --type node | `--bench 1` | 71.3 s | `047ac2f9` | |
| inspect --tags --type way | `--bench 1` | 82.9 s | `959bda7c` | |
| inspect --tags --type relation | `--bench 1` | 8.8 s | `8daf5f04` | |
| inspect --extended | `--bench 1` | **820.7 s (13m41s)** | `19db1512` | full-decode era; superseded on bygg by "Inspect --extended: validator-projection scan" below |
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
> group is swept on a worker thread with bounded streaming. The 133-minute
> row predates all three; the callouts below hold the cells measured since.
> A cell now has more to read than that row: `MERGEPBF_*` now nests
> `MERGEPBF_PASS1_*`, `MERGEPBF_PASS2_*`, `MERGEPBF_WRITE_LOOP_*` and
> `MERGEPBF_FLUSH_*`, the `cat_dedupe_*` counters give the plan's and the
> write's blob, run and duplicate counts, and `--stalls` attributes pass 2:
> `ordered_pool_result_recv` is the write loop waiting on a sweep,
> `ordered_pool_result_send` a sweep waiting on the write loop, and
> `ordered_pool_job_lock` plus `ordered_pool_job_recv` sweep workers idle
> for want of an issued run.

> **`cat --dedupe` planet with itself, borrowed-cursor sweep (2026-10-30,
> bygg).** Baseline `d271f04f` (`627883d7`, owned-element heap sweep,
> 4-block channel per run): **53m43s, peak anon 7.57 GB**, 3.2 avg cores,
> `ordered_pool_result_send_wait_ns` 88,527 s (about 27 of 30 workers
> parked behind the write loop holding their heaps). Variant (the commit
> after `42ea567f`, which replaced the heap with a k-way merge over
> borrowed cursors and framed each run's blocks on its own worker, sent in
> 2 MiB chunks), measured uncommitted with `--force` so not stored:
> **5m10s (310.6 s), peak anon 1.46 GB**, write loop at 27.3 avg cores,
> result send wait 0, result recv wait 303 s (the write loop now waits on
> the sweeps). 10.4x faster and 5.2x less peak anon. Same plan on both
> (50,816 two-blob runs, 0 passthrough). No fstrim before the variant cell.

> **What the planet self-merge sweeps hold (2026-11-04, bygg, `fda05956`
> plus the change that added the `overlap_run_*` counters, measured
> uncommitted with `--force`).** The counters (`src/commands/blob_merge/sweep.rs`,
> "Resource counters") on `cat --dedupe` planet with itself, cell
> `10f5ec37` (dirty sidecar): largest run charge
> `overlap_run_pinned_high_water_bytes` **12.5 MB** (two twin blobs, about a
> fifth of `SWEEP_PINNED_BYTES`), `overlap_run_demoted_cursors` 0,
> `overlap_run_chunks` 63,466 over 50,816 runs, largest chunk 2,097,145
> bytes (the 2 MiB budget), largest completed output held before shipping
> 3.9 MB. Matched pair after `fstrim`, interleaved `--bench 1`: HEAD
> `8827bc7f` 207.9 s and `78832185` 205.6 s against the variant 206.6 s and
> 208.1 s, signs mixed, write loop +1 % in `--compare`, peak anon about
> 1.0 GB on both: the bookkeeping costs nothing measurable.

> **Large frames sent in chunk-sized pieces (2026-11-04, bygg, `8a2c8ad2`
> plus the change that frames an over-chunk frame straight into pieces of
> at most `OVERLAP_RUN_CHUNK_BYTES`, measured uncommitted with `--force`).**
> `cat --dedupe` planet with itself, one `--bench 1` cell each: HEAD
> `2202ef70` 201.5 s, variant `c57ba39b` (dirty sidecar) 206.3 s. The
> write loop, the only phase the change touches, is 1 % faster in the
> variant (198.9 s against 200.7 s); the wall difference is pass 1, which
> read 654 MB from disk in the variant against 19 MB (a cold header walk,
> code the change does not touch). Planet zlib output has no frame above a
> chunk, so every frame is one piece and the counters are identical
> (63,466 chunks, largest 2,097,145 bytes): no cost on normal input.

> **Same-ID groups ship block by block, dedupe lookup indexed past a
> threshold (2026-11-08, bygg, `5903b4cd` plus the change, measured
> uncommitted with `--force`).** `cat --dedupe` reindexed planet with
> itself, interleaved `--bench 1`, no fstrim: HEAD `33e06a0f` 201.9 s,
> variant 201.4 s (dirty sidecar `b01f27ad`), HEAD `a6d92ca1` 200.3 s,
> variant 200.8 s (dirty sidecar `33f2ce2b`). Signs mixed, under 1 %.
> Counters unchanged (12.5 MB pinned, 63,466 chunks, staged 3.9 MB):
> planet groups are two copies, below the index threshold and inside one
> block, so normal input takes the old path.

> **Denmark and Japan across the engine move (2026-11-08, bygg, kernel
> 7.2.14).** `cat --dedupe` of each dataset's indexed file with itself,
> `dd312160` (the old engine, parent of the move) against `79d2e923` (the
> move: `HeaderWalker` pass 1, overlap runs swept on dedicated threads)
> against HEAD `4e0eaa50`, every arm a `--commit` worktree build (so
> brokkr#105's build offset cancels). Script bench `dedupe-self`
> (`benches/cat/dedupe-self.lua`), one iteration per cell, the input's page
> cache fixed and fincore-checked as the merge starts: cold (evicted) or
> warm (wholly resident). Arms rotated across rounds, denmark cold all six
> orders, the other conditions three. Walls in seconds; per cell, range
> across cells:
>
> | Condition | `dd312160` | `79d2e923` | `4e0eaa50` |
> |---|---|---|---|
> | denmark cold (6 cells) | 28.1-28.9 | 2.8 | 1.3-1.4 |
> | denmark warm (3) | 27.9-28.9 | 2.1-2.2 | 1.2 |
> | japan cold (3) | 141-142 | 13.0-13.1 | 6.1-6.5 |
> | japan warm (3) | 140-141 | 9.4-9.6 | 5.2-5.7 |
>
> The move made the self-merge about 10x faster on Denmark and 11-15x on
> Japan; everything after it to HEAD (the borrowed-cursor sweep `13ec0927`
> among it, not isolated here) halved it again. Cold costs the old engine
> nothing measurable (its cold and warm walls overlap); at HEAD, cold
> Japan's pass 1 is 0.92 s against 0.04 s warm (`2bcd9315` against
> `91bf8ad9`), almost all of that cell pair's 0.89 s gap; pass 2 took
> 5.61 s against 5.60 s although it read from disk cold. Japan
> peak anon: 89 MB, 1.18 GB, 368 MB for the three arms (`1524888a`,
> `83fc73eb`, `91bf8ad9`); the move's sweep threads bought the speed with
> memory, and HEAD gives most of it back. Output is the same size on every
> arm (denmark 495,905,546 bytes, japan 2,417,611,089), and `--check yes`
> runs (`19f2ce5f`, `89f2ce1e`, `f6d7708e`) diff each arm's denmark output
> against its input with no difference. The iteration wall includes about
> 12-15 ms of fincore and eviction before the merge, about 1 % of the
> fastest (denmark HEAD) cells and less elsewhere.
> What this does not measure: the self-merge puts every blob in a two-blob
> run, so passthrough and its coalesced `copy_file_range` never run; the
> next callout does.

> **Denmark passthrough across the engine move (2026-11-09, bygg, kernel
> 7.2.14, ext4, `sudo fstrim -av` first).** Script bench `dedupe-pair`
> (`benches/cat/dedupe-pair.lua`): denmark base merged with snapshot
> `one-relation`, a 436-byte file holding only base's highest-id relation
> (`getid r20243795`). One overlap run of two relation blobs; the other
> 7,395 base frames pass through, at HEAD as one coalesced copy (counters:
> 1 copy call, 7,394 frames coalesced, 486,570,984 bytes, 0 fallback
> ranges), at `dd312160` as a read and `write_raw` per frame. Same three
> arms, HEAD now `f49dd460`, all `--commit` worktrees, one iteration per
> cell, arm order rotated across three cold rounds and two warm ones.
> Iteration walls in ms (the cat child is about 25 ms less):
>
> | Condition | `dd312160` | `79d2e923` | `f49dd460` |
> |---|---|---|---|
> | cold (3 cells) | 389, 327, 333 | 1069, 1073, 1045 | 513, 518, 460 |
> | warm (2) | 215, 212 | 183, 200 | 193, 211 |
>
> Warm, the three arms overlap: the coalesced copy neither wins nor
> loses wall on this file, as the sort coalescer `244c6ec` did not on
> Europe. Cold, HEAD is about half again the old engine's wall (median
> 513 against 333 ms) though far below the move's. The cat child of
> HEAD cell `e4484a26` (435 ms) splits into pass 1 at 177 ms (the header
> walk) and pass 2 at 253 ms, 244 of it the copy's flush; the old cell
> `4f376168` (305 ms) is one span at 0.6 s user and 0.6 s kernel. The
> suspected cause, an inference not shown by disk-read attribution: the
> old pass 1 skipped payloads through a 256 KB `BufReader` with
> sequential readahead, which prepared the cache for its pass 2, while
> HEAD's header-only walk leaves the copy to fetch the file cold. Output
> is 486,848,104 bytes on every arm, and `--check yes` cells on each arm
> (HEAD `4f3830d1` dirty-tree, `e069805a`, `0900f601`) diff it against
> base with no difference. `--with ways-only` is the mixed shape (every
> way blob overlaps, nodes and relations pass through: 6,568 frames, 2
> copy calls at HEAD); it is not timed here.

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

### inspect full-decode scan on the decode pipeline (bygg, 2026-10-30)

`inspect --extended` (and every other full-decode inspect mode) used to
decode, validate and fold every element on the calling thread. It now runs
on the ordered read pipeline: each data blob is summarized on a decode
worker into a per-blob scan state, folded in file order on the consumer.
Planet (`planet-20260223-with-indexdata`), `--bench 1`:

| cell | commit | wall | peak anon | avg cores |
|---|---|---:|---:|---:|
| baseline `9b49e8fb` | `627883d7` | 13m22s | 26 MB | 1.0 |
| parallel scan (unstored `--force` cell) | landing commit, tree at `13ec0927` + change | **41.4 s** | 571 MB | 29.3 |

The run is worker bound: the dispatcher spends 96 % of wall waiting for
an admission permit (`pipeline_decode_admit`), the decoded-channel send
wait is 0.5 %. The memory is the 30 workers' transient decode storage
(decompress buffers, offset tables, allocator retention), not retained
summaries: the permit window holds summaries, which are small (tens of KB at
most, under `--locations`).

### inspect --extended metadata census by value (bygg, 2026-10-30)

The census judges each user name by what it resolves to. Resolving the name
per element (a string-table load for every object) cost about 5 % on planet
`--extended`; precomputing, once per block, which string indices are empty
removes the per-element load. Matched interleaved `--bench 1` cells,
baselines stored at `9a2c0275` via `--commit HEAD`, variants unstored
`--force` cells on that tree plus the change:

| cell | wall |
|---|---:|
| baseline `ae2927ba` | 41.76 s |
| per-element resolve | 43.82 s |
| baseline `dd1d937c` | 42.07 s |
| per-element resolve | 43.97 s |
| per-block empty-index table | 42.31 s |
| baseline `ee5cc078` | 42.36 s |
| per-block empty-index table | 42.59 s |

The per-block table is within noise (+0.6 % against the adjacent baseline in
both pairs); phase split identical (29.3 avg cores, same disk read). It
landed; the per-element resolve did not.

### Decode buffers sized from the blob and recycled (bygg, 2026-10-30)

The pread decode workers asked the per-worker `DecompressPool` for
`4 x compressed size` (about 7.2 MB for a 1.8 MB planet blob), Vec
doubling on the inline offset tables grew other buffers past the pool's
4 MiB retention cap, and so every planet buffer was dropped on return
(drops = scheduled blobs in every cell): one multi-MB allocate-and-free
per blob per worker. mallinfo2 sampled mid-run in getparents at
`c97b9985` showed 175-250 MB in use against 280-420 MB free inside the
arenas, all resident (THP `always` on bygg). Now the decode helpers size
the buffer from the declared `raw_size` with exact growth (tables
included), callers pass no guess, and the pool retains buffers up to
16 MiB under a 256 MiB idle budget per pool: zero pool drops at planet.
The apply-changes workers recycle theirs through a per-worker pool too.

Once decode buffers stop being freed, nothing raises glibc's sliding mmap
threshold early, and time-filter's in-worker build and frame buffers went
through mmap/munmap per blob: main phase 139.9 s to 169.1 s, kernel time
1.8 to 5.7 cores. The CLI's allocator hook then pinned
`M_MMAP_THRESHOLD` at 32 MiB beside its two-arena cap, which recovered
it. Commands without the hook (getparents, extract, tags-filter,
apply-changes) showed no work-phase regression. The churn was costly only
under the arena cap: with glibc's default arenas time-filter runs the same
wall with or without the pin, and the CLI now sets neither (see "The CLI
no longer caps malloc arenas").

Planet (`planet-20260223-with-indexdata`), `--bench 1`, peak anon RSS.
Variant cells are unstored `--force` cells on `c97b9985` plus the change.
Wall compares the work phases (sidecar phase split): the variant cells ran
with the schedule header walk out of page cache (about 6 s per walk,
code unchanged), which the stored baselines had cached.

| command | baseline | peak before | peak after | work phases |
|---|---|---:|---:|---|
| `getparents` | `dd453b3f` | 705 MB | 288 MB | decode 13.41 s to 13.29 s |
| `check --ids` | `4edab6b2` | 646 MB | 566 MB | -1 % / -2 % |
| `time-filter` | `4f093a45` (`c97b9985`, same day) | 1,177 MB | 841 MB | 139.9 s to 139.0 s |
| `cat --clean version` | `a8a4384e` | 1,242 MB | 994 MB | nodes -4 %, ways -2 % |
| `apply-changes` (4913, `zstd:1`) | `f2b8beed` | 3,123 MB | 2,965 MB | streaming 120.0 s to 120.4 s |
| `extract --simple` (`-25,34,45,72`) | `6b3c88cc` | 3,522 MB | 3,094 MB | equal or faster per phase |
| `tags-filter w/highway=primary` | `b8491cfd` | 2,845 MB | 2,568 MB | equal per phase |
| `inspect --extended` | unstored 571 MB cell | 571 MB | 355 MB | 41.4 s to 42.2 s |

What is left in the larger peaks is not decode storage: apply-changes'
overlay (575 MB by `merge_diff_heap_bytes` as it was then, an
allocated-capacity estimate, not resident memory; its parts are counted
in "Apply-changes retained lookup storage") and
drain reorder (415 MB high water; since bounded by a
sequence window that does not bind on this input, see "Apply-changes
sequence window"); the
persistent ID sets of extract, tags-filter and check `--ids --full`. The
`pwrite` pool's queue is unquantified: no counter samples its occupancy,
and on the default splice route it queues rewritten output frames, which
average about 146 KB at `f2b8beed` (`627883d77`, bygg;
`writer_payload_raw_bytes` over `writer_payload_raw_items`), not the
1.8 MB input blobs.

### Worker-kept decode buffers follow the pool limit (bygg, 2026-11-03, against `d4c97d42`)

Every plain decode buffer a worker keeps across blobs outside a pool (the
raw scan behind `check --locations`, altw sparse and sorted
`extract --simple`; altw external stages 1, 2 and 4; altw passthrough's
way path; geocode pass 1.5; apply-changes' refresh-scan buffer) now takes
the decode pool's per-buffer limit once per blob through
`release_decode_outlier`, counted in
`pipeline_decompress_pool_oversize_drops`. The sequential take loops
(altw external stage 4's non-way blobs, the unsorted extract sweeps) keep
handing their buffer to the block unpooled and now reuse their parse
scratch.

On planet the limit never fires: zero oversize drops in both variant cells
below, as for the pooled paths. Variant cells are unstored `--force` cells
on `d4c97d42` plus the change, `--bench 1`.

| command | baseline | wall before | wall after | notes |
|---|---|---:|---:|---|
| `check --locations` (`altw-keep-untagged`) | `c17025ee` (`7feb4370`; same-commit cells 203.9-208.1 s) | 206.7 s | 201.8 s | every partition phase -1 % to -8 % |
| `add-locations-to-ways --index-type external` | `a12a2f7d` (`8ccb3a2e`; same-commit cells 317.0-353.5 s) | 317.0 s | 322.1 s | stage 4 127.1 s to 122.6 s, `s4_nonway_assemble_ms` 623,354 to 607,361; stage 1 32.4 s to 36.2 s |

The altw stage 1 delta is run history, not the change: the variant
followed a `check --locations` cell over a different 147 GB file, and the
untouched phases before and beside it moved with it (blob metadata scan
5.2 s to 7.9 s, relation scan 9.9 s to 11.6 s, disk read in the segment
after pass A starts 11,373 MB to 13,517 MB). Stage 1's code change is one capacity compare per blob.

### Worker read buffers follow a retention limit (bygg, 2026-11-04, against `22a5f24a`)

Every buffer that keeps a compressed blob payload or a whole frame across
blobs is now `crate::read::read_buf::ReadBuf`: exact growth (the old
`resize` doubled, so a buffer kept up to nearly twice its largest frame)
and a release past `READ_BUF_MAX_RETAINED_CAPACITY` once per blob, counted
in `pipeline_read_buf_releases`. The direct reads' aligned scratch follows
the same line. Two lines were measured on `check --locations` over
`planet-20260223-altw-keep-untagged` (way frames median 4.5 MB, max
9.6 MB), `--bench 1`, run order HEAD, HEAD, 4 MiB, 16 MiB, 4 MiB, HEAD
(HEAD cells stored, variant cells `--force`):

| arm | walls (s) | max phase anon (GB) | releases |
|---|---|---|---|
| `22a5f24a` (`d030ae1a`, `b91bc0b6`, `9cb69c5a`) | 197.4, 203.2, 204.2 | 17.65, 18.14, 17.96 | - |
| 4 MiB, the scratch limit (sidecars `eea69fb4`, `28ac76b5`) | 214.4, 222.9 | 17.98, 18.10 | 90,972 |
| 16 MiB, landed (sidecar `48fea30c`) | 204.4 | 17.99 | 0 |

The 4 MiB line freed and regrew a way-frame buffer on most way reads (452 GB
of released capacity over the six partitions); partitions 2 to 5 ran 2 to
4 % slower in both of its cells, partition 1 17 and 39 %, and the run peak
did not fall. The 16 MiB line runs phase for phase as HEAD and never fires
on planet. apply-changes keeps its own 4 MiB line on the base-read buffer
(see "apply-changes' worker scratch follows the release rule" below).

`add-locations-to-ways --index-type external` on the indexed planet
(largest frame 3.7 MB, zero releases), variant then HEAD: 325.5 s
(`7a756d41`) against 356.4 s (`c0b11c81`). Stage 2 159.2 against 158.9 s,
stage 4 123.8 against 121.7 s; the wall gap is the HEAD cell's stage 1 pass
A at 67.9 s against 33.9 s here and 32 to 36 s in earlier records, run
history rather than the change.

### apply-changes frames each block as it completes (bygg, 2026-10-30)

A planet input blob holds about 200k elements and is rewritten into about
27 output blocks (8,000 elements each). The workers built all of them as
raw `OwnedBlock`s and framed them only after the whole blob was rewritten,
so each of the 30 workers held a blob's whole raw output beside its framed
copy. Now the rewrite hands each block to a framing sink as soon as the
builder completes it (after each element or create), so one block's raw
bytes are resident at a time. The frames still stay on the worker until
the blob's walk has ended.

Planet (`planet-20260223-with-indexdata`), OSC 4913, `--compression
zstd:1`, `--bench 1`, 30 workers. The variant is an unstored `--force`
cell on `5bfa37ab` plus the change.

| cell | wall | peak anon | streaming cores |
|---|---:|---:|---|
| `f2b8beed` (`627883d7`, stored) | 127.1 s | 3,123 MB | 27.7 |
| `5bfa37ab` (unstored, previous section) | - | 2,965 MB | streaming 120.4 s |
| `5bfa37ab` + per-block framing | 125.1 s | 2,364 MB | user 28.1, kernel 1.2 |
| the same + `cross_thread_alloc_pool` | 2606 s | 2,252 MB | user 3.3, kernel 23.3 |

The last row is why apply-changes never requested the CLI's allocator
hook (two arenas plus the pinned 32 MiB mmap threshold): twenty times the
wall, nearly all of it kernel time, for 112 MB less peak. The arena cap
alone causes it, and the hook is gone; see "The CLI no longer caps malloc
arenas".

### The CLI no longer caps malloc arenas (bygg, 2026-11-01)

The CLI answered the library's cross-thread allocation hook with
`mallopt(M_ARENA_MAX, 2)` and a 32 MiB `M_MMAP_THRESHOLD` pin. Both are
gone, with the hook. Cells are unstored `--force` cells of one binary,
`7a99298f` plus a switch choosing which settings the hook applied (cap,
pin, both, none); apply-changes, which never requested the hook, took
the same settings from `GLIBC_TUNABLES` on the same binary. `--bench 1`,
32 threads, glibc 2.43.

Which setting. germany apply-changes (OSC 4705, `zstd:1`), streaming
phase: none 1.36 s, pin only 1.33 s, cap only 4.94 s, both 5.07 s;
kernel 3.0 cores without the cap, 18.4 with it. germany cat `--clean`
walls: none 7.4 s, pin only 7.4 s, cap only 17.1 s, both 17.2 and 17.5 s.
The cap is the whole cost; the pin is neutral. At planet (OSC 4913,
`zstd:1`) apply-changes with both settings still takes 2608.9 s
(`MERGE_STREAMING` 2603.0 s, user 3.4 and kernel 22.9 cores, peak anon
2,335 MB), against 126.9 s (`MERGE_STREAMING` 120.3 s, user 26.6 and
kernel 1.3 cores, 2,389 MB) in the stored cell `188db700` at
`26fe5a5e`: the 2606 s of the previous section reproduces after the
sequence and delivery windows.

Mechanism. `strace -f -c` of the germany apply-changes run: 1,125,285
`futex` calls with the cap against 186,934 without, while `madvise`
(138 against 164), `mmap`, `munmap` and `mprotect` did not grow, nor did
minor faults in the sidecar (about 96k either way), so the time is not
trimming, mapping or faulting. With two arenas, every allocation above
the tcache limit (about 1 KiB) from 30 workers and the writer threads
takes one of two arena mutexes (glibc's `lll_lock`: futex-backed, no
user-space spin), and for an arena-backed chunk `__libc_realloc` holds
the mutex across `_int_realloc`, including the allocate, copy and free of
a block it has to move (`malloc/malloc.c`). apply-changes allocates 12.3 GB on
germany (`--alloc`, 4.7 GB input), mostly framing (447 KB average per
call), block take-out (379 KB) and rewrite (230 KB) buffers, so its pool
runs as two lock convoys. Where in the kernel the extra 15 cores go was
not traced (no perf on the host); the futex count says it is contention
handling.

Planet (`planet-20260223-with-indexdata`), wall and peak anon, hook
(both settings) against none:

| command | with the hook | without |
|---|---:|---:|
| `cat --clean version` | 237.2 s, 971 MB | 150.6 s, 918 MB |
| `build-geocode-index` | 320.1 s, 13,296 MB | 175.5 s, 13,319 MB |
| `repack` | 268.9 s, 1,371 MB | 170.9 s, 1,934 MB |
| `degrade --strip-locations` | 268.0 s, 1,384 MB | 170.4 s, 1,932 MB |
| `cat --dedupe` | 258.6 s, 1,096 MB | 217.0 s, 1,101 MB |
| `time-filter` | 147.5 s, 802 MB | 134.1 s, 761 MB |
| `renumber` | 125.3 s, 3,159 MB | 124.3 s, 3,324 MB |
| `check --refs` | 36.4 s, 2,216 MB | 36.4 s, 2,226 MB |
| `check --ids --full` | 28.3 s, 2,318 MB | 28.3 s, 2,295 MB |

Pin only: time-filter 134.0 s, 811 MB; repack 169.5 s, 1,697 MB. Removing
the cap cut the wall of every measured command that encodes or builds per
blob by 9 to 45 % (the cap added 10 to 82 %), and in each the hooked
cell's work phases carried more kernel time (up to 1.7 cores in
time-filter, up to 12 to 17 in cat `--clean`, repack, degrade and the
geocode builder, against at most 1 without); the read-only checks and renumber's splice rewriters were
unaffected. The memory the cap was added for no longer
appears: renumber's call site recorded about 26 GB uncapped and the
geocode builder's 25 GB, against 3.3 GB and 13.3 GB measured here. The
one peak the cap still lowers is repack's and degrade's relation phase
(about 4 s of the run), 1.38 GB against 1.93 GB, which is left to the
phase itself rather than bought with 98 s of wall. Not measured at
planet: `check --locations`, the sort sweep on an input with runs, and
`check --ids` without `--full`.

### Encode storage kept across blocks (bygg, 2026-11-01, against `5789830e`)

The change cells ran as uncommitted edits on `5789830e`; the change
landed unaltered as the commit after it.

The change: one string table per builder over a byte arena (no
allocation per string, where the old table made an `Rc<str>` per unique
string and freed them all at every block end); ordered-pool workers of
`cat --clean`, the time-filter snapshot path, repack and degrade keep one
`BlockBuilder` for the run and frame each block from its buffer; framing
hands out one exactly sized frame instead of swapped-out header and body
parts that the writer flattened; `take_owned` leaves a capped reservation
for the next block. Outputs of cat `--clean`, repack (cap 8000 and 3000),
degrade and time-filter on germany are byte-identical to `5789830e`.

Arena cap (MALLOC_ARENA_MAX=2) on germany cat `--clean`: `5789830e` 16.9 s
(`42a26ca1`) against 7.4 s uncapped; the change without its string table
15.2 s (8.49 M futex calls at `5789830e` against 7.07 M); with it 7.4 s,
the uncapped wall. The per-blob vectors were a tenth of the cap's cost;
the string table's per-string allocations were the rest.

Planet, interleaved `--bench 1` pairs, clean `5789830e` cells (stored)
against the uncommitted tree (`--force`, not stored); walls in s, peak anon
in MB:

| command | `5789830e` wall | change wall | `5789830e` peak | change peak |
|---|---:|---:|---:|---:|
| cat `--clean version` | 151.1, 153.3 (`f687bd33`, `870e72f6`) | 154.0, 155.3 | 913, 898 | 838, 824 |
| repack | 172.8, 170.6 (`f078a43a`, `43c3f0fc`) | 170.2, 169.0 | 1,934, 1,896 | 1,790, 1,804 |
| degrade `--strip-locations` | 170.5, 171.9 (`e39f176d`, `32b0d647`) | 170.7, 171.3 | 1,914, 1,920 | 1,797, 1,775 |
| time-filter | 134.3, 133.8 (`35a53690`, `ed16f718`) | 132.3, 131.8 | 766, 778 | 720, 719 |
| getparents | 17.4, 15.6 (`a77c851b`, `4ca09dc3`) | 17.4, 15.7 | 283 | 277, 286 |
| apply-changes (4913, `zstd:1`) | 129.5, 128.0, 126 (`86cd89b7`, `3e2ebea2`, `c6312436`) | 106.8, 106.5, 103.3 | 2,435, 2,304, 2,399 | 2,663, 3,270, 3,187 |
| tags-filter (two-pass) | 76.6, 73.2 (`5513882e`, `c9788558`) | 74.2, 73.1 | 2,548 | 2,553, 2,550 |
| multi-extract `--simple` (5 regions) | 128.0, 127.4 (`5d0090fc`, `58f8c8e1`) | 128.6, 127.1 | 9,989 | 9,914, 9,853 |
| cat `--dedupe` | 217.1, 217.7 (`74c1a641`, `c072eb75`) | 215.6, 216.4 | 1,088, 1,086 | 1,131, 1,121 |
| renumber | 131.6, 129.1 (`e785cfa9`, `6091aff9`) | 123.8, 133.2 | 3,314 | 3,331, 3,316 |

Under MALLOC_ARENA_MAX=2 at planet: cat `--clean` 234.5 s (`f38d9391`)
against 153.0 s, repack 266.9 s (`377506c5`) against 173.2 s - the
change's capped walls equal its uncapped ones.

Walls are flat within noise except apply-changes, 17 % faster: an
apply-changes cell of the change with the old `Rc<str>` table ran 124.6 s
and peaked at 2,508 MB, so the wall comes from the arena table. Peaks fall
5 to 8 % on the four commands whose workers now keep a builder; cat
`--dedupe` is about 3.5 % higher. apply-changes' peak rises 0.3 to 0.9 GB,
in a step at the node-to-way transition (51 to 83 threads in both builds)
that stays resident to the end. The table does not hold it: the step is
allocator free memory left by a transient burst there, and the arena table
changes only how much of the burst the arenas' existing free memory
absorbs. "apply-changes' memory step at the node-to-way transition" below
attributes it.

### cat --clean's peak against the README row (bygg, 2026-11-03, at `1f93e674`)

Planet `cat --clean version`, stored cell `6f9896db`, `--bench 1`, 30
workers: 146 s, 766 MB peak anon. The peak rises through the run and is set
in the `CAT_RELATIONS` segment, the last (segment peaks 289 MB in the pool's
first 112 ms, then 627, 720 and 766 MB). The README's 750 MB is `f2315551`
(plantasjen, `4fc8e35`, the per-kind phases under the two-arena cap, 25
threads at peak against this cell's 33, so 22 workers against 30); this cell
is 2 % above it with 36 % more workers, inside the noise floor. The
encode-storage cells above (824 and 838 MB) ran before the string table
moved into the encode buffer and the worker decode buffers took the pool
limit; which of those moved it was not measured.

The ordered pool's 64-task window does not hold it. Stall counters:
`ordered_pool_result_recv_wait_ns` 130.4 s (89 % of the wall, the consumer
idle for want of a result), `ordered_pool_window_wait_ns` and
`ordered_pool_result_send_wait_ns` 0. The consumer only writes raw frames,
so results in flight stay near one per worker and the window binds only
behind a slow head. Zero window wait says the admission window did not bind
in this run; consumer receive wait is not worker idle time, so it does not
prove no result ever waited behind a slow head. The peak is the workers' own
working sets: decode, read and encode buffers and frames of the blob in
hand.

### cat --clean across the single ordered pass (bygg, 2026-11-09, `a05b3d25` vs `8da26b23`)

Planet `cat --clean version` on `planet-20260223-reindexed`, the parent
(three per-kind phases, a `ReorderBuffer` each) against the commit that made
it one ordered pass with a private 64-slot credit window. Both arms built as
`--commit` worktrees (brokkr#105 cancels), `--bench 1`, A B B A, untrimmed:

| Cell | Commit | Wall | Peak anon | Avg anon |
|---|---|---|---|---|
| `8b442cca` | `a05b3d25` | 229.3 s | 1,043 MB | 941 MB |
| `5cba4939` | `8da26b23` | 231.4 s | 1,104 MB | 902 MB |
| `91b5ebba` | `8da26b23` | 233.3 s | 1,097 MB | 871 MB |
| `c726040f` | `a05b3d25` | 228.7 s | 1,077 MB | 959 MB |

The one pass is 0.9 % and 2.0 % slower in the two pairs, below the noise
floor: no measurable net cost. Peak anon is 2-6 % higher and average anon
4-9 % lower, the per-kind phases' tails replaced by one steady pool. Phase
split (`8b442cca` vs `5cba4939`): schedule walk 6.2 vs 5.4 s, the work 223.1 s
across the three kind phases vs 226.0 s in `CAT_FILTERED`, both at 26-27
cores. Writer stalls match: `writer_recv` 88.5 % of the wall on both,
`writer_pipeline_send` 12.8 vs 12.3 s. Both arms predate `25172fc3`, so both
ran under the two-arena malloc cap; that is why they sit near 230 s against
the 146 s at `1f93e674` above.

Not answered: whether the 64-slot window stalled workers behind a slow head
at `8da26b23`. Its credit gate parks the dispatcher on a condvar and counts
nothing (`ordered_pool_*_wait_ns` arrive with the engine in `c2409c45`), and
a net-neutral wall does not rule out a stall offset by savings elsewhere.
With no regression to attribute, the historical gate was not instrumented.

### Dense-node direct encoding, refused (plantasjen, 2026-11-05, against `9ebcfa97`)

`BlockBuilder` stages each dense packed field in a scratch buffer, the
DenseInfo submessage in `elem_scratch` and the DenseNodes body in
`group_buf`, then copies the body once into the encode buffer. The variant
measured every packed body first (a varint-width pass over each dense
vector, checked sums), reserved the whole block once (string table plus
group) and wrote every field straight into the encode buffer. A unit test
against the staged path and a germany `cat --clean version` byte compare
(62,461 blobs, `cmp` clean) showed identical output.

Planet `cat --clean version`, `--bench 1`, interleaved H V V H, after
`fstrim`. H is stored (`41ca4be3`, `f9b8146d`); V ran `--force`, sidecars
`046eceb9` and `095013e5`.

| Cell | Wall | `CAT_NODES` | `CAT_WAYS` |
|---|---|---|---|
| H `41ca4be3` | 296.4 s | 179.2 s | 107.1 s |
| V `046eceb9` | 311.0 s | 190.4 s | 109.6 s |
| V `095013e5` | 312.2 s | 192.0 s | 109.3 s |
| H `f9b8146d` | 304.4 s | 183.7 s | 110.1 s |

The node phase, the only one the change touches, is 5-6 % slower in both
pairs; the way phase is flat. Node-phase peak RSS fell 25-40 MB, inside the
noise floor. About 180 us per 8000-node block across the workers: computing
a width per value costs more than bulk-copying bytes that are already
encoded. Not adopted; the staging stays. Untried shapes: accumulating packed
lengths as values are added, and reserving prefix space then compacting.

### repack and degrade encode each blob on its worker (bygg, 2026-11-03, against `1f93e674`)

**Attribution** (uncommitted diagnostic on `1f93e674`, sidecar session
`4ca7a7a9`, 163.7 s, peak 1,734 MB): the repack merge thread emitted
`mallinfo2` beside the bytes held in worker results not yet consumed (frame
capacities, and a heap estimate of the owned elements with each allocation
rounded to glibc's chunk size), every 4 relation, 128 way and 256 node
results.

- Planet relation blobs hold about 31k relations each (452 blobs, 14.1 M
  relations), so at the default cap of 8000 each worker result was about
  3.4 framed full blocks plus about 3,700 relations shipped as owned
  `OwnedRelation`s. The merge thread re-encoded every one of those
  remainders, and because the ordering guard flushed its builder at every
  blob, framed each on its own, synchronously. The phase ran 3.9 s at 8.3
  to 8.8 cores against about 29 in the node and way phases.
- At the relation phase's start the live heap (`uordblks`) was 1.8 MB, with
  348 MB free in the arenas. 0.3 s later 77 results were in flight, holding
  707 MB of owned remainders and 134 MB of frames; live heap 1.42 GB, arena
  1.71 GB. Then 50 to 60 results, 250 to 500 MB of remainders and about
  1.0 GB live to the phase's end. So the relation-phase peak was live data
  queued behind the merge thread, not retention.
- The way phase is worker-bound (no send wait), about 0.6 GB live in a
  1.14 GB arena. The node phase's decode buffers are mmapped chunks
  (`hblkhd` 435 to 489 MB, arena about 216 MB); they are freed when its pools
  end, which raises glibc's mmap threshold, and the way phase's buffers then
  come from the arenas. Not changed here directly, but see the way-phase
  column below.

**The change**: a worker encodes and frames every element of a repack blob
holding at least the cap, the remainder as its own block, and ships only
frames; a blob below the cap still ships owned elements, which coalesce in
the merge thread's builder. degrade outside an unsort mode, whose boundary
flush already made every remainder its own block, encodes every blob on its
worker. The merge thread's blocks go to the pipelined writer, which frames
them on its executor in sequence order, so the synchronous batch framing
(and its `FRAME_BATCH` tunable) is gone. degrade's output is unchanged;
repack's differs only where a blob of at least the cap is followed by blobs
below it, whose elements no longer join its remainder (planet: no coalesce
in any phase, 1,453,673 blobs either way). degrade's worker outside an
unsort mode no longer walks its blob once to count it before encoding, and
repack's takes the count from the block's validated element count when the
block holds no other kind (every indexed blob), walking only an unindexed
one.

Planet (`planet-20260223-with-indexdata`), `--bench 1`, HEAD cells stored
from brokkr's worktree, change cells unstored `--force` cells of the tree on
`1f93e674` (sidecar sessions named); run order repack HEAD, change, degrade
HEAD, change, repack HEAD, change, degrade HEAD, change, with no trim
between (two short denmark cells ran before the second degrade HEAD cell):

| arm | walls (s) | peak anon (MB) | relation phase | way phase peak (MB) |
|---|---|---|---|---|
| repack `1f93e674` (`85aad3a2`, `bbb4d0b6`) | 163.2, 164.6 | 1,718, 1,712 | 3.9-4.1 s, 8.3-8.4 cores | 956, 957 |
| repack, change (`77386e44`, `736f2416`) | 161.3, 160.2 | 777, 781 | 1.4 s, 27.8 cores | 777, 781 |
| degrade `--strip-locations` `1f93e674` (`d89acb69`, `1ebee79b`) | 164.4, 164.0 | 1,728, 1,707 | 3.9 s, 8.4-8.7 cores | 962, 958 |
| degrade `--strip-locations`, change (`4a458795`, `db35770b`) | 152.3, 152.7 | 791, 789 | 1.4 s, 29.3 cores | 791, 789 |

degrade's node phase ran 92.1 and 92.7 s against 99.5 s in both HEAD cells,
the counting walk it no longer makes over 32,835 blobs of about 318k nodes;
its relation phase saves the other 2.5 s. The two repack change cells above
still made that walk; a third, after repack took the validated count
(`776bcb32`, run last, after degrade's second change cell), ran 148.3 s at
800 MB, node phase 88.7 s against 99.3 to 100.1 s in every other repack
cell, 1,453,673 blobs.

Output on denmark (`denmark-20260220-seq4704-with-indexdata`), the change's
binary against `1f93e674`'s: byte-identical for degrade `--strip-locations`,
for repack at the default cap and for repack of the 64k-per-blob snapshot at
the default cap. Repack at `--elements-per-blob 3000` differs where the
rule changed: 22,184 blocks against 22,183 (one more way block, where the
last way blob's elements no longer join the remainder before them), the
same 59,152,282 elements by `pbfhogg diff`, strict type-then-ID order.

The way phase peaks lower too, although its work is the same in both
builds (its blobs hold exact multiples of the cap, so neither has a
remainder there). In the HEAD cells the merge thread framed the node
phase's one remainder (its last blob's) through the executor, which started
its 32 threads before the way phase (65 threads at peak there, against 33
in the change cells, which start none at planet). Those idle threads held
the node workers' freed arena memory; see the next section.

### Idle executor threads take exited workers' arenas (bygg, 2026-11-04, against `a82e23bc`)

**Mechanism.** glibc gives a thread its malloc arena at the thread's first
allocation, and `arena_get2` takes an arena off `free_list` (arenas whose
every thread has exited, pushed by `__malloc_arena_thread_freeres`) before
it creates one (`research/glibc/malloc/arena.c`). Every rayon worker
allocates as it starts (rayon-core 1.13: `main_loop`, `WorkerThread::from`,
`JobFifo::new`, a crossbeam `Injector` block, before the worker reports
itself started). So the CLI's executor, rayon's global pool started at its
first job, takes whichever arenas a just-ended worker pool left, free
memory and all, if its first job comes after that pool ended.

**Attribution** (uncommitted diagnostic: repack dumped per-arena
`malloc_info` at the node phase's end, after an `executor.spawn` of an
empty job there, and at the way phase's midpoint; planet
`planet-20260223-with-indexdata`, `--bench 1`, sidecar sessions `7dc1d843`
and `ec9d7037`):

- At the node phase's end 30 node-worker arenas held 316 MB, 314 MB of it
  free.
- Lazy start (the CLI as it is): at the way midpoint those 30 arenas were
  unchanged to the byte, held by the 32 idle executor threads, and 30 new
  arenas had grown to 21-36 MB each for the way workers: 1,130 MB held in
  65 arenas. Way phase peak anon 1,055 MB, relation phase 974 MB, wall
  147.7 s.
- The CLI building the global pool before dispatch (`build_global`, which
  waits until every worker has started): the 32 executor arenas stayed at
  0.13 MB each, the way workers took the 30 node arenas (about 11 to 26 MB
  each) and no arena was created: 787 MB held. Way peak anon 758 MB,
  relations 689 MB, wall 147.3 s.

That is the 180 MB of the section above, measured there at `1f93e674`.

**Starting the pool early is no general cure.** The same early start
measured on planet `build-geocode-index` (interleaved, `a82e23bc` from
brokkr's worktree `410c8832`, `87ea7677`, against unstored sessions
`7b56aa31`, `32ba68ea`): walls 173.8 and 171.1 s against 171.7 and
173.2 s, peak anon 13,299 and 13,302 MB against 13,309 and 13,323 MB, but
every phase from `GEOCODE_PASS2_ADMIN_ASSEMBLY` (the builder's first
executor job, after its pass 2 scan pools ended) to the end peaked 320 to
420 MB higher in RSS, p50 anon 4.67 and 4.78 GB against 5.07 and 5.18 GB.
There the lazily started executor inherits the pass 2 workers' arenas and
does the heavy parallel work in them; started early it grows its own while
those sit unused. Which order wins depends on whether the pool that starts
after a worker set ends then works or idles, so the CLI keeps the lazy
start and the `Executor` rustdoc states the consequence for library
callers.

**The change**: what the executor was started for at repack's phase
boundaries was one block, the phase's residual after its pool ended. On an
input whose blobs hold exactly the cap, the last input blob of each kind is
below it, so its elements go to the merge thread's builder and its final
flush was the executor's first job. The merge thread now frames that
residual itself and writes it raw. Output is byte-identical
(`denmark-20260220-seq4704-with-indexdata`, at the default cap and at
20000). degrade has no such block outside its unsort modes. Under those
every element goes through the merge thread's builder, which usually starts
the executor during the first phase; an input whose first phase completes
no block before the final flush still starts it after the pool ends. Not
changed: degrade's unsort modes produce adversarial test inputs.

Planet written at 8000 elements per blob (`--snapshot 8k`), `--bench 1`,
`a82e23bc` from brokkr's worktree (stored) against the change (unstored
sessions), run order HEAD, change, change, HEAD:

| arm | walls (s) | peak threads, way phase | way phase peak anon (MB) | peak anon (MB) |
|---|---|---|---|---|
| `a82e23bc` (`7d437f6d`, `41ab9a1c`) | 255.4, 256.8 | 65 | 583, 496 | 687, 619 |
| change (`7ac2425b`, `40ef3705`) | 260.0, 255.7 | 33 | 479, 458 | 580, 593 |

The node, way and relation phases run the same time in all four (88.3 to
89.0, 51.4 to 51.8 and 1.33 to 1.36 s); the 4.4 s wall difference in the
first pair is the schedule scan (114 against 118 s), which the change does
not touch. Germany (`germany-20260224-seq4704-with-indexdata`), one cell
each, the lazy trigger at HEAD (`17a4601d`) against an early-started pool
(session `ecf95439`): way phase peak anon 224 against 206 MB.

**The other executor users** (surveyed 2026-11-05 at `d919ab21`). Where
the executor's first job falls after a worker set ends:

- Its whole run or final pass, working throughout: `cat` (passthrough and
  `--type`, no worker set before), `time-filter` history, `getid`,
  `getparents`, two-pass `tags-filter`, `add-locations-to-ways` (every
  index type's output pass), `renumber` (every stage frames every block
  on it), unsorted simple extract (no worker set before its batches).
  `merge-changes` and the OSC `tags-filter` run their whole body inside
  it. `sort`, `cat --clean` and `time-filter` snapshot hand it nothing.
- Between worker sets, then idle: extract's pread write pools, which simple
  extract on sorted input alternates with its classify pools (node
  classify, node write, way classify, way write, relation classify,
  relation write). The executor started at the node write, after the node
  classify workers exited, and sat idle through each later classify phase.
  A node write whose blobs are mostly verified raw copies would start it
  for a few rebuilt blocks, the repack shape.

**The change**: `pread_execute` (simple's three write phases and the
complete and smart write passes) frames each block on its worker through
`FramedBlocks` and writes the frames raw, so extract's pread passes hand
the executor no job. Planet (`planet-20260223-reindexed`), `--strategy
simple --bbox=-25,34,45,72`, `--bench 1`, plantasjen, order HEAD, change,
HEAD:

| arm | walls (s) | peak threads, write / classify phases | peak anon (MB) |
|---|---|---|---|
| `d919ab21` (`bb82cdd8`, `4a447ac3`) | 223, 224 | 49 / 50 | 2,842, 2,855 |
| change (unstored session) | 226 | 25 / 26 | 2,400 |

Every phase runs the same time in both (node write 100.5 to 102.1 s, way
write 56.6 to 58.2 s); average cores in the write phases fall from about
23 to 21.5, the framing pool's threads that ran beside the workers. Output
passes `brokkr verify extract` on denmark; the frames carry the same
indexdata, tagdata and compression as the writer's (the builder's blocks
carry no WayMembers). degrade's unsort modes are left as above.

### apply-changes' memory step at the node-to-way transition (bygg, 2026-11-02, against `2cd85f7e`)

Planet (`planet-20260223-with-indexdata`), OSC 4913, `--compression
zstd:1`, `--bench 1`, 30 workers. All cells are unstored `--force` cells
built in the main tree (the clean arm by checking out the changed paths).

**Attribution** (diagnostic build of `2cd85f7e`: `mallinfo2` and per-arena
`malloc_info` from the drain loop every 5 s and around each type
transition, markers around the transition; 99.4 s, peak 3,142 MB):

- The 32 threads are rayon's global pool, built lazily by the drain's
  first block framed on the writer's executor: the OSC's node creates past
  the last base node blob, written at the node-to-way transition (309
  blocks, about 2.47 M creates; nothing in apply-changes used rayon before
  it). Their 32 arenas hold under 2 MB each.
- The transition took 335 ms on the drain thread, encoding those creates
  one by one. Meanwhile the workers kept producing framed way output
  behind it: live heap (`uordblks`) went 1.058 to 1.772 GB during it, and
  back to 1.05-1.10 GB within three seconds, where it stayed to the end.
  The heap (`arena`) grew 1.772 to 2.498 GB and free heap (`fordblks`)
  0.715 to 1.41-1.45 GB, both flat to the end; anon peaked at 3.14 GB 1.6 s
  after the transition and settled at 2.75 GB.
- Per arena, the 725 MB of heap growth is spread over the 30 worker arenas
  and main (5 to 54 MB each), and in each the growth is free memory, mostly
  in 0.5-2 MB chunks. So the step is allocator retention of a transient
  backlog, not live data and not the new threads; it is a one-time high
  water and does not grow with the input after it.
- With the old `Rc<str>` table swapped into the same build (122.2 s, peak
  2,547 MB) the burst is the same (+0.62 GB live over a 431 ms
  transition), but the heap grows only 0.30 GB: more of it is served from
  free memory the arenas already held.

**The change**: every create run of the drain (gap creates, the creates
left at a type transition, the trailing creates) is cut into blocks of the
element cap, as one builder filling to the cap would cut it, encoded and
framed in parallel on the invocation's executor when it spans more than one
block (`stream_output::CreateEncoder`), and written raw in order. The
node-to-way transition went from 335 ms to 66 ms, and the live growth over
it from +0.71 GB to +0.13 GB. Output blocks are unchanged
(`tests/apply_changes_create_runs.rs` pins the cuts).

Interleaved cells, in run order change, HEAD, HEAD, change, change, HEAD,
HEAD, change:

| arm | walls (s) | peak anon (MB) | step at the transition |
|---|---|---|---|
| `2cd85f7e` | 98.9, 104.8, 99.7, 104.6 | 2,655, 3,063, 3,075, 2,546 | +0.48 to +0.80 GB where read |
| change | 98.9, 100.0, 105.8, 100.6 | 2,737, 2,251, 2,702, 2,524 | +0.06 to +0.60 GB |

Three diagnostic cells of the change (instrumented as above) ran 99.2,
99.2 and 99.4 s and peaked at 2,301, 2,942 and 2,169 MB. Walls do not
move; the median peak falls from about 3.06 to about 2.5 GB. One change
cell (2,702 MB) reached its plateau (2.58 GB) at t=15 s in the start-up
ramp; its recorded peak came at the end of the run, 0.12 GB above that
plateau, not at the transition.

**Residual, not removed**: the step does not always vanish. A 100 ms
diagnostic of the change shows the drain writer-bound through the node
phase (blocked sending to the writer for 50 to 100 ms of every 100) and
worker-bound once ways start (send wait near zero, reorder-gap wait
instead). In one cell live heap then rose 1.01 to 2.13 GB over 1.6 s with
the drain's reorder map near empty and the drain advancing about 400 seqs
per second, fell back to 1.1 GB, and left 0.8 GB free; in another nothing
of the kind happened (live 1.04-1.13 GB, anon +0.1 GB). Its owner was
measured later and removed: rewritten blobs queued in the drain channel
while the drain was writer-bound, in the node phase and at the transition,
not after way blobs start ("apply-changes' drain channel held the node
phase's backlog" below).

### apply-changes' rewrite workers frame from their builder (bygg, 2026-11-02, against `cbf48cc8`)

The change cells ran as uncommitted edits on `cbf48cc8`; the change
landed unaltered as the commit after it.

The change: each rewrite worker frames every output block straight from
its builder's buffer (`commands::FramedBlocks`, `BlockBuilder::lend_encoded`)
instead of moving the encode buffer out (`take_owned`) and framing the
owned copy, so one allocation and free of about a raw block per output
block is gone; `lend_encoded` drops an encode buffer grown past the
retention limit once the block is framed, for every lender. Output bytes
are unchanged (same encoder, same frame arguments).

Planet (`planet-20260223-with-indexdata`), OSC 4913, `--compression
zstd:1`, `--bench 1`, after `fstrim`. Both arms built in the main tree
(the clean arm by checking out the changed paths); run order change,
HEAD, HEAD, change, change, HEAD, HEAD, change:

| arm | walls (s) | peak anon (MB) |
|---|---|---|
| `cbf48cc8` (`72569ba1`, `2e184e4e`, `aef81466`, `84a9c263`) | 101.3, 112.2, 99.2, 103.3 | 2,199, 2,560, 2,763, 2,678 |
| change | 96.2, 96.2, 101.5, 100.7 | 2,167, 2,327, 2,686, 2,503 |

Medians 102.3 against 98.5 s and about 2,620 against 2,415 MB, with the
ranges overlapping and one adjacent pair reversed: within the noise this
page names for the session's other buffer changes. Minor faults in the
streaming phase run 0.33-0.48 M on the change against 0.42-0.55 M on
`cbf48cc8`. `merge_streaming_frame_ns` (framing only, excluding the
encode, on both arms) is unchanged at about 660 core-seconds. The change
landed as a simplification, not for its numbers; the arenas' free heap
was not instrumented, so how much of apply-changes' residual step these
buffers fed is not attributed.

### apply-changes' drain channel held the node phase's backlog (bygg, 2026-11-02, against `eeb7ab6c`)

Planet (`planet-20260223-with-indexdata`), OSC 4913, `--compression
zstd:1`, `--bench 1`, 30 workers.

**Attribution** (uncommitted diagnostic: one thread sampling `mallinfo2`
and the bytes each stage holds every 100-120 ms: candidates queued,
decode buffers in use, worker `read_buf` capacity, idle decode-pool
buffers, frames of a rewrite in progress, results in workers blocked
sending, results in the drain channel, the reorder map, the item the drain
is writing, frames past `write_raw_owned` not yet written by the pool;
beside the cumulative writer send wait and drain gap wait). Two cells,
sidecar sessions `dbf409a4` and `dce4df1c`:

- The variable burst was the drain channel. It reached 244 and 246 items
  holding 0.94 and 0.80 GB of rewrite results (about 4 MB each: every
  frame one input blob's rewrite produced), against an empty channel at
  other times. Live heap followed it (0.9 GB with the channel empty, 1.7 to
  1.9 GB at its peaks) and the heap kept what it grew (arena 1.58 to 2.04
  and 2.46 GB). The reorder map reached about 0.27 GB at its high water
  (stored counters of `dbf409a4`), workers never blocked
  sending, and frames past the writer's handoff held 5 to 25 MB (up to
  120 MB for a moment at the node-to-way transition).
- It happens in the node phase, the start-up ramp included, and at the
  transition, never after it. Through the node phase the drain is
  writer-bound (writer send wait accrues 0.7 to 1.0 s per second, gap wait
  none), so whenever the workers briefly out-produce the writer their
  results queue in the channel, whose 256 slots were the only limit
  (candidates 65, workers 30 and the channel stay under the 512-seq
  window). After the transition the drain is worker-bound and the channel
  stays empty to the end; when in the node phase the backlog builds, and
  how far, is what varied between runs.
- Worker scratch is steady, not the burst: decode buffers in use 100 to
  160 MB, `read_buf` capacity 150 to 170 MB, idle pool buffers 0 to
  150 MB.
- Not attributed then: live heap beyond every counted stage rises from
  about 0.53 to 0.80 GB over the late node phase and the transition, and
  stays. It is the workers' retained encode and framing storage
  ("apply-changes' retained encode storage, attributed" below).

Under `-l` (`planet-20260223-altw-keep-untagged`, session `5fd7df66`, 300 ms
sampling) the drain is worker-bound for the whole run and the channel
never showed more than 10 items. The step there at the node-to-way barrier
is the way phase's larger working set and stays: `read_buf` capacity 157 to
351 MB and decode buffers in use 175 to 300 MB (way blobs with locations
are larger), plus about 250 MB of other live growth, the published
coordinate map among it.

**The change**: `DRAIN_CHANNEL_CAPACITY` 256 to 32. Workers block sending
while the drain is behind the writer, which costs nothing there because
the writer is the bottleneck; the drain still receives every message
whatever the reorder state, so no seq can be held back. A diagnostic cell
at 32 (`03547df0`) held at most about 140 MB in the channel, live heap
peaked at 1.08 to 1.28 GB, and no head starvation was observed.

Interleaved cells after `fstrim`, run order change, HEAD, HEAD, change,
change, HEAD, HEAD, change:

| arm | walls (s) | peak anon (MB) |
|---|---|---|
| `eeb7ab6c` (`a722a405`, `d268c8e3`, `5fe46b45`, `38e4e0a6`) | 89.6, 99.0, 89.1, 95.7 | 2,275, 2,995, 2,376, 2,868 |
| change | 89.8, 89.4, 96.0, 89.1 | 2,038, 2,090, 2,017, 2,151 |

Walls overlap (medians 92.7 and 89.6 s, not a speedup); the median peak
falls from about 2.62 to about 2.06 GB and its spread from 0.72 to
0.13 GB.

Where the scanner's splices share the smaller channel with rewrite
results, germany (OSC 4705, 82 % of blobs spliced, `--bench 3`, order
change, HEAD, HEAD, change) ran 3.01 and 3.00 s against 3.05 and 3.06 s
(`b0a5b51a`, `cffb4f7c`). Under `-l`, one pair: 261.8 s against 258.5 s
(`44becd1a`), within noise, as the channel there never fills.

### apply-changes' worker scratch follows the release rule (bygg, 2026-11-03, against `149944a1`)

The change: every buffer an apply-changes worker keeps across blobs
(`WorkerScratch` in `streaming.rs`) is checked once per blob against
`MAX_RETAINED_SCRATCH_CAPACITY` (the decode buffer a clean refresh scan
keeps, against the decode pool's limit); the base-read buffer grows by a
fresh exact allocation instead of `resize`'s doubling; the barrier merge
takes each worker's coordinate slot table whole, so it is freed instead of
being drained and kept to the end of the run.

Planet, OSC 4913, `--bench 1`, after `fstrim`, every arm built in the main
tree (HEAD cells stored, variant cells `--force`). Two variants of the read
buffer's limit were measured: a 16 MiB line that keeps every ordinary frame,
and the common 4 MiB limit, which landed.

`--locations-on-ways` on `planet-20260223-altw-keep-untagged`, run order
16 MiB, 4 MiB, HEAD, HEAD, 4 MiB, 16 MiB:

| arm | walls (s) | peak anon (MB) |
|---|---|---|
| `149944a1` (`e520509f`, `5b8be372`) | 263.6, 264.6 | 3,210, 3,244 |
| 16 MiB (sidecars `a50f57df`, `8dc7b811`) | 266.1, 266.1 | 3,115, 3,127 |
| 4 MiB, landed (sidecars `be19da76`, `1f7568cf`) | 265.4, 263.2 | 3,023, 3,008 |

Walls are level (the 16 MiB arm's 2 s is under the noise floor). The 4 MiB
line released the read buffer 15,162 times in 50,814 reads (17,273
allocations against 202 at 16 MiB: about every way blob, whose frames
under LocationsOnWays exceed 4 MiB), at no wall cost, and lowered the peak
about 100 MB further than the 16 MiB line, both pairs on the same side. The
summed per-worker read-buffer peaks are 253 and 251 MB, against about 351 MB
of doubled buffers in the `eeb7ab6c` diagnostic; the barrier freed 134 MB of
slot tables (`merge_drain_barrier_slot_bytes_freed`) that had stayed live
beside the published map. Peak anon falls about 200 MB (6 %) from HEAD. That
the released capacity lowers the peak is measured under glibc on this run,
not a property of the rule.

`--compression zstd:1` on the indexed planet, run order 16 MiB, HEAD, HEAD,
HEAD, 16 MiB, 16 MiB, 4 MiB:

| arm | walls (s) | peak anon (MB) |
|---|---|---|
| `149944a1` (`9eeb759c`, `86c9ec6d`, `f8421d6a`) | 92.6, 99.7, 96.6 | 2,105, 2,060, 2,078 |
| 16 MiB (sidecars `6c0e9cb7`, `b522e35e`, `44a0b2c2`) | 92.3, 91.6, 99.1 | 2,136, 2,106, 2,093 |
| 4 MiB, landed (sidecar `56649031`) | 92.6 | 2,059 |

Neutral. No candidate frame there exceeds 4 MiB (zero releases at either
line, so the two variants run identically); the read buffers' summed peak
is 100 MB against the diagnostic's 150 to 170 MB, below what the peak
resolves. The workers' other kept scratch (group, node-group and refresh
scratch and the coordinate accumulator, `merge_streaming_scratch_peak_bytes`)
peaks at 30 MB summed over 30 workers (38 MB under `-l`).

### apply-changes' retained encode storage, attributed (bygg, 2026-11-03, against `062d7cce`)

Planet (`planet-20260223-with-indexdata`), OSC 4913, `--compression
zstd:1`, `--bench 1`, 30 workers.

**Attribution** (uncommitted diagnostic, two cells, sidecar sessions
`95f7de9d` and `b7026a58`, which agree): one thread sampling `mallinfo2`
every 200 ms beside the retained capacity of every owner the earlier
diagnostic did not count, each reported by its owner as a delta into a
process-wide atomic: every worker's `BlockBuilder` after each blob (split
into dense vectors, wire scratch with `group_buf` and `elem_scratch` apart,
string table bytes, slots and index, encode buffer), each framing thread's
`FrameScratch` after each frame (blob, header and compression buffers, and
the zstd context's `sizeof`), by thread role (worker, drain, executor),
and the drain's own create builder. The earlier stages were counted again:
frames from `frame_blob_pipelined` until the parallel writer's pool wrote
them (rewrite results in progress, queued, reordered and past the writer
handoff in one figure), decode buffers in use and idle in the pools, read
buffers and the workers' small scratch. Capacities, not occupied bytes;
`uordblks` also holds chunk overhead and tcache chunks, so the remainder is
approximate.

Summed over the 30 workers, MB, steady node phase / end of the node phase
/ way phase:

| owner | node | late node | way |
|---|---|---|---|
| `BlockBuilder` total | 320-334 | 455 | 503-532 |
| of it: string table bytes, slots, index | 59, 55, 17 | 111, 58, 18 | 111, 60, 18 |
| of it: `elem_scratch` | 71 | 112 | 112 |
| of it: `group_buf` | 17 | 22 | 59-83 |
| of it: encode buffer | 70-75 | 80-87 | 94-100 |
| of it: dense vectors | 36-38 | 44.5 | 44.5 (unused in the way phase) |
| `FrameScratch` buffers | 104-111 | 128 | 153-164 |
| of it: compression buffer, blob buffer | 79, 31 | 95, 33 | 97, 56-65 |
| zstd contexts | 17.5 | 17.5 | 17.5 |

The 32 executor threads built at the node-to-way transition hold 24 MB of
frame scratch and zstd contexts between them; the drain's builder 0.5 MB.
What is left of `uordblks` after every counted stage and these owners is
40 to 110 MB and flat for the whole run. So the earlier figure, 0.5 GB
through the node phase rising to 0.8 GB, is this storage: 0.45 GB once the
workers have seen their first blocks, growing as later node blocks (larger
string tables: rewritten blocks keep their input block's whole table) and
then the way phase's buffers raise each buffer's high water. Every buffer
is under the 4 MiB release limit, so the rule works as designed; this is
the working set of 30 kept builders. Not counted: the up to 32 per-chunk
create builders an executor batch holds at the transition, live only
during the batch (counted at their end only).

Peak anon is set at the node-to-way transition: frames in flight burst to
about 290 MB on top of this retained floor and the arena grows from 1.62
to 1.84 GB, which stays.

**The change**: two of these buffers held only a copy of bytes another
retained buffer also held, and are gone. `StringTable::encode_to` writes
the string table straight into the encode buffer behind a length computed
from its slots, instead of staging the body in `elem_scratch` (which at
planet was held at the size of the largest table a worker met), and the
dense-node group is wrapped behind lengths computed from its body, one copy
instead of two. `FrameScratch` keeps only the Blob body's few head bytes:
the payload is read from the compression buffer (or the uncompressed
block) when the frame is assembled or written, instead of being copied
into a blob buffer first. Output bytes are unchanged.

Interleaved main-tree cells after `fstrim`, order B, A, H, H, H, B, A, B,
H, A, B (H = `062d7cce`, A = the string table and dense wrap alone, B =
both changes):

| arm | walls (s) | peak anon (MB) |
|---|---|---|
| `062d7cce` (`cd47ef65`, `7b961746`, `a764940a`, `6c65750e`) | 97.0, 103.7, 96.5, 95.6 | 2,032, 1,994, 2,148, 2,029 |
| builder change alone (sidecars `fab96364`, `d02589ea`, `e35898fb`) | 95.3, 95.6, 98.5 | 1,903, 1,951, 2,071 |
| both, landed (sidecars `0a74edf9`, `cae243bf`, `0fc8b513`, `b0b18472`) | 91.2, 92.8, 92.6, 97.7 | 1,917, 1,842, 1,927, 1,908 |

Every cell of the landed change peaks below every HEAD cell: median about
2,031 against 1,913 MB, -118 MB (6 %). Walls are not slower (medians 96.7
and 92.7 s; not claimed as a speedup). The framing change's own increment
over the builder change alone, about 38 MB of peak and 3 s of wall (faster
in every adjacent pair), is under the noise floor on both counts; it
landed as the removal of a duplicate retained buffer and a payload copy on
every framing thread, not for its numbers.

What remains of the retained storage is each buffer's share of one block,
the rest of which no change here touches: the encode buffer, the
compression buffer (reserved at the compression bound of the raw block),
way and relation groups staged in `group_buf` before the string table
ahead of them is final, the string table itself, and dense vectors kept
through the way phase (44.5 MB). Each of the last two is about 2 % of the
peak.

### apply-changes checks the string indices it copies (bygg, 2026-11-03, against `59802597`)

The change: every string index a rewrite copies raw is checked against the
source block's table (dense and plain node tag pairs and the dense user,
already decoded; a way's or relation's values and roles in one varint pass
each, its `Info` user by a scan for field 5, its keys inside the decode the
tagdata tracking already made), and every flush resets the builder.

Planet (`planet-20260223-with-indexdata`), OSC 4913, `--compression
zstd:1`, `--bench 1`, every arm built in the main tree (HEAD cells stored,
variant cells `--force`), no trim of its own (the drive was trimmed that
morning). Run order HEAD, first form, HEAD, first form, landed, HEAD,
landed. Rewrite CPU is `merge_streaming_rewrite_ns` summed over the 30
workers (encoding included, framing excluded).

| arm | walls (s) | rewrite CPU (s) | peak anon (MB) |
|---|---|---|---|
| `59802597` (`efcd42fa`, `7be19856`, `f9ba5b99`) | 91.8, 91.8, 97.0 | 1,090, 1,092, 1,057 | 1,891, 1,942, 1,908 |
| first form (sidecars `b742ac42`, `4143d48c`) | 94.0, 97.8 | 1,155, 1,132 | 1,942, 1,869 |
| landed (sidecars `c486073d`, `d7f1ce89`) | 91.9, 91.1 | 1,076, 1,080 | 2,010, 1,929 |

The first form left the builder unchanged on a refusal (dense pairs staged
and walked twice), parsed each copied `Info` whole and checked keys in a
pass of their own: about 50 s more rewrite CPU (43,671 of 50,816 blobs are
rewritten, so most of the planet's 11.6 billion elements are copied), +4.5 %
wall, above the noise floor. The landed form poisons the
builder on a refusal instead, so each array is walked once, and reads only
the `Info` user; its walls and rewrite CPU are inside HEAD's spread. The
`--locations-on-ways` path, which copies ways through the same checks, was
not measured.

### apply-changes' create-run builders are made per block (bygg, 2026-11-03, against `cf73c10f`)

The question: a create run longer than one block is encoded on the
executor with a fresh `BlockBuilder` per block (about 309 blocks in five
batches at the planet node-to-way transition), where the re-encoding
workers keep one builder for the run. Does building one per block cost
wall or memory?

Both arms carry a `MERGE_TRANSITION_CREATES_START`/`_END` marker pair
around the creates written at each type transition (landed with this
entry; earlier cells have no transition marker). Arm A is `cf73c10f`
plus the markers. Arm B adds an invocation-owned pool to `CreateEncoder`
(`Mutex<Vec<BlockBuilder>>`: a task takes a builder or makes one, and
returns it once its block is framed), so no more builders exist than tasks
ran at once and each is reused across blocks, batches and transitions.

Planet (`planet-20260223-with-indexdata`), OSC 4913, `--compression
zstd:1`, `--bench 1 --force`, both arms built in the main tree, no trim of
its own. Run order A B B A A B B A A B; the last B cell queued behind
another project's brokkr command. The span is the node-to-way transition's
`MERGE_TRANSITION_CREATES` pair.

| arm | sidecars | walls (s) | peak anon (MB) | transition span (ms) |
|---|---|---|---|---|
| A, a builder per block | `0e3f915f`, `64550e04`, `41e140e0`, `1883d680`, `f95cbd01` | 100.2, 94.7, 98.5, 93.8, 97.3 | 1,983, 1,972, 1,882, 1,873, 1,869 | 33, 34, 16, 22, 15 |
| B, pooled builders | `74bd551c`, `e585ee57`, `8d601702`, `068a40b4`, `b5ca7fc0` | 95.0, 99.9, 96.1, 99.2, 95.0 | 1,902, 1,878, 1,884, 1,896, 1,906 | 44, 28, 45, 40, 27 |

Medians: wall 97.3 against 96.1 s, peak anon 1,882 against 1,896 MB, span
22 against 40 ms. The pool shows no wall or sampled-peak benefit. Its
spans were longer in four of five adjacent pairs; that is an observed
direction, not an established regression, and at tens of milliseconds a
run it would not matter either way. The per-block builders stay.

Limits of this record. The sidecar samples every 100 ms, so it cannot
reliably resolve an instantaneous peak inside a 15-45 ms span: the builders'
share of the transition's live burst is still not attributed, only shown
not to move the run's sampled peak (set after the way-to-relation
transition, at the end of the run, in every cell). The first two A cells show a second step of
about 85 MB in anon about 0.8 s after the transition, which no other cell
shows; run history is the likelier cause, but an occasional allocator
effect of the per-block frees is not excluded by ten cells. `map_init`
and per-task groups of contiguous blocks were not tested. A thread-local
builder was not tested because it is ruled out on other grounds: the
executor's threads belong to the caller and outlive the invocation.

### apply-changes' create-run loop lifted into a function its tests drive (bygg, 2026-11-04, against `3c5b8d3a`)

`CreateEncoder::emit`'s batching and ordered-write loop moved, unchanged,
into `write_run` (`src/commands/apply_changes/stream_output.rs`), which
takes the block encoder and the frame writer as closures so unit tests can
fail chosen blocks and record what is written. The closures are
monomorphized; no change was expected, and one cell checks that.

Planet (`planet-20260223-with-indexdata`), OSC 4913, `--compression
zstd:1`, `--bench 1`. Baseline is the stored cell at `3c5b8d3a`
(`ed178984`: wall 96.4 s, peak anon 1,999 MB, transition spans 11.3 and
33.8 ms); the change, uncommitted on `43373aa0` with `--force` (sidecar
`7eab1838`): wall 93.9 s, peak anon 1,995 MB, transition spans 18.1 and
21.1 ms. Within noise on every figure; the phase split matches phase for
phase.

### tags-filter checks stored tagdata is canonical (bygg, 2026-11-03, against `ebe69342`)

The change: `TagIndex::deserialize`, which runs once per blob header on a
tag-filtered read, refuses a key not greater than the one before it (one
slice compare per key in the loop that already copies each key), a key
count the bytes cannot hold, and bytes after the last key.

Planet (`planet-20260223-with-indexdata`), the default `w/highway=primary`
expression, which skips most blobs on their tagdata, `--bench 1`, both arms
built in the main tree, no trim of its own. Run order variant, HEAD.

| arm | wall (s) |
|---|---|
| `ebe69342` (`53d5f705`) | 77.7 |
| variant (`--force`, sidecar `24f24eb9`) | 77.2 |

Neutral: the variant is inside a single cell's noise.

### Parallel-array iterator ends are checked in release (bygg, 2026-11-03, against `ebe69342`)

The change: the end arm of every lockstep iterator over an element's
parallel arrays (`TagIter`, `RawTagIter`, `WayNodeLocationsIter`,
`RelMemberIter`, `MemberIdIter`, `DenseNodeIter`) checks in release that
all its arrays ended together, through one inlined branch onto a cold,
never-inlined panic with a static message; it was a `debug_assert!`. The
same tree also carries the other invariant-check dispositions of that day
(all per block, blob, chunk or run).

Planet (`planet-20260223-with-indexdata`), the default `w/highway=primary`
expression, `--bench 1`. Its way passes skip no blob on tagdata
(`tagsfilter_single_pass_tagidx_skipped_blobs=0`), so every way's tags are
walked through `TagIter`, twice. The A arm is the stored main-tree cell of
the previous entry, whose `src/read/elements.rs` and `dense.rs` are
identical to the variant's base `9c4d6c81`; the variant is `9c4d6c81`
plus the change, built in the main tree. No trim of its own.

| arm | wall (s) |
|---|---|
| `ebe69342` (`53d5f705`) | 77.7 |
| variant (`--force`, sidecar `32433dfa`) | 74.1 |

No cost: the variant is faster, by an amount within the run-to-run spread
these cells show, so no gain is claimed either.

### Element parse and re-encode refusals checked in release (bygg, 2026-11-08, `5a9d40e4` vs `85c1428c`)

The pair brackets exactly three changes (`5a9d40e4` itself touches only
altw external): `e28fdf69` (`take_message` refuses a second data-bearing
`Info` or `DenseInfo` at element parse; the `info()` accessors panic on a
broken invariant instead of defaulting), `9c85d37e` (the block iterators
and the per-node dense `keys_vals` walk go through `validated`, an inlined
branch onto a cold panic, instead of skipping or reading less) and
`85c1428c` (the re-encoding paths in `src/owned.rs` return a `Result` per
element for the user lookup and per member for the role, through
`relation_members_into`).

Planet (`planet-20260223-reindexed`), both arms `--commit` worktree builds
(the same build path, so brokkr#105's worktree offset cancels), `--bench 1`,
arms alternated A B B A, kernel 7.2.14. `brokkr read` runs the element parse
and iterator checks but never touches `info()` or a re-encode; `repack`
re-encodes every element with its metadata (10.45G nodes, 1.17G ways,
14.1M relations through `relation_members_into`, 98.4 GB written), so it
runs every `85c1428c` site. A way-only `tags-filter` was rejected as the
re-encode cell: it keeps no relation and never reaches the role loop.

| cell | `5a9d40e4` (s) | `85c1428c` (s) | delta |
|---|---|---|---|
| read sequential, round 1 | 512.3 (`a5bbad97`) | 508.4 (`a3bdd79a`) | -0.8 % |
| read sequential, round 2 | 512.9 (`a139aae7`) | 508.3 (`314f89ac`) | -0.9 % |
| read parallel, round 1 | 28.6 (`6ce94499`) | 28.7 (`65019195`) | +0.2 % |
| read parallel, round 2 | 30.2 (`16bf1999`) | 30.2 (`b8df6cad`) | 0.0 % |
| repack, round 1 | 260.5 (`1000e419`) | 261.8 (`65d789b4`) | +0.5 % |
| repack, round 2 | 263.5 (`2d9e0431`) | 264.6 (`00e308e4`) | +0.4 % |

No measurable cost on any cell: every delta is under 1 %, inside the noise
floor, and both rounds agree. This is a screen for a regression, not a
proof of a bound: the pair can say the checks cost nothing visible at
planet, not that their cost is under any figure. The verdict is for the
pair; the checked functions are unchanged since, but the code around them
is not, so it is not re-claimed for later trees without a cell there.

### The partial-index guard, not measured by `brokkr read` (bygg, 2026-11-08, at `71c0a4d3`)

`84f50b31` armed every command-input frame walk against a partially
indexed file: an armed walk parses the 42-byte indexdata, deserializes it
and observes it once per completed data frame, including on readers whose
caller asked not to keep it. The proposed screen, `brokkr read --dataset
planet` at `84f50b31^` against `84f50b31`, was not run because it cannot
see the guard: `bench-read` opens every mode through the public library
constructors (`ElementReader::open_with` and `BlobReader::open_with` at
HEAD, `from_path_with_eviction` at `84f50b31`), which never arm the walk,
so both arms run with no guard. `par_map_reduce` has no armed caller.

On the armed command paths the added work is fixed per frame: a stack
copy and a scalar `BlobIndex::deserialize`, with no allocation, element
walk, payload read or seek. These figures are a source estimate, not a
measurement. At 100 ns a frame, a planet walk of 50,816 frames costs about
5 ms and a europe walk of 522,168 frames about 52 ms. At 1 us a frame the
same walks cost about 51 ms and 522 ms. Either way the cost sits far below
the noise floor of a minutes-long planet run. Inputs packed more densely
than europe, and short header-only walks, have a different denominator,
so this is not a claim of insignificance everywhere.

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

### altw external raw scanners read fallibly (bygg, 2026-10-31)

`e61e0ad7` replaced the external join's zipped `PackedSint64Iter`s (which
swallowed decode errors) with three `read_varint`s in lockstep. `brokkr
add-locations-to-ways --dataset planet --index-type external --bench 1`,
`--commit` cells, after `sudo fstrim -av`, two pairs in opposite order:

| Pair | `2ba3e170` (parent) | `e61e0ad7` | Delta | UUIDs |
|---|---:|---:|---:|---|
| 1 (parent first) | 369.9 s | 388.7 s | +5.1 % | `b3cc51d6` `ae2ba447` |
| 2 (change first) | 360.1 s | 352.3 s | -2.2 % | `2cda149e` `764b7ba2` |

Pair 1 is confounded by cell position: stage 1 pass A took 32.4 s in the
first cell (the first after the trim) and 96.9 s in the second, more than
the whole wall delta, while stage 2 ran 13 % faster on the change. This
was first read as the relation scan swinging (20 s against 87 s); that
figure is the `--human` row labelled `EXTJOIN_RELATION_SCAN_END`, which is
the segment that begins at that marker, the rest of pass A. The scan's own
START/END span is 12.0 and 10.3 s. In pair 2 both cells follow another
altw cell, pass A is 65.7 and 62.7 s, and stage 1, 2 and 4 are level or
slightly faster on the change. The whole-command walls cannot resolve a
cost below the position effect; the phase split shows none. Next section
for the position effect.

### altw external pass A swings with run history (bygg, 2026-10-31)

`brokkr add-locations-to-ways --dataset planet --index-type external
--bench 1`, `8ccb3a2e`, two cells back to back (about 6 s apart) right
after a `sudo fstrim -av`:

| Cell | Wall | Pass A | Relation scan | `s1a_pread_ms` | `s1a_id_shard_write_ms` | UUID |
|---|---:|---:|---:|---:|---:|---|
| 1 (first after trim) | 316.9 s | 32.4 s | 9.9 s | 147,288 | 558,332 | `a12a2f7d` |
| 2 | 353.4 s | 73.5 s | 12.7 s | 598,979 | 1,318,963 | `4cd2eddf` |

Pass A explains the whole 36.5 s wall delta; stage 2 and stage 4 are
within a few seconds. Pass A's work is byte-identical in every cell: 29.2
GB pread in 17,529 calls and 149.2 GB of id shards written in about
573,000 calls; only the time inside those calls moves (thread-cumulative
ms above). The same split holds across the twelve planet cells of
2026-10-30/31: the relation scan's span is 7-13 s in every one, pass A is
32-33 s in the cells that follow a different command, a long idle or a
trim (`13668dda`, `cf58d02e`, `b3cc51d6`) and 51-111 s in every cell that
follows another altw external cell or a large writer.

It is not input page cache. Per-process `/proc` io for `b3cc51d6` (fast)
and `ae2ba447` (slow, 5 s later) shows both reading about 28 GB from disk
during pass A, the whole way section; the slow cell did not lose cached
input and the fast one did not reuse any (the host has about 32 GB of RAM).
What differs is the rate at which pass A dirties shard pages: about 4.6
GB/s against 1.5 GB/s, steady across the pass. That is a dirtying rate
under kernel throttling, not a measured device rate. Each planet cell
writes and deletes hundreds of GB of scratch on an ext4 mount without
`discard`, so the freed blocks stay untrimmed until the next `fstrim`.
Trim debt, an exhausted drive write cache, controller GC and inherited
writeback state are not separated by these cells; that would need a trim
between consecutive cells, with device throughput and Dirty/Writeback
recorded.

Consequence for measurement: on this host, an altw external cell that
follows a scratch-heavy cell has run 30-80 s slower in pass A than one
that follows a trim or rest (11.5 % of wall in the pair above). A matched A/B must balance what precedes
each arm (predecessor workload and first-after-trim position), not only
alternate arms; when the arms write different scratch volumes, alternation
alone does not balance it. Read pass A against the rest of the command
before attributing a wall delta to code. Pass A's scratch volume was more
than it needed to be: the packed id-shard record below cuts it by a third.
The trim-between-cells chain further below ("pass A after a trim or after
another cell") narrows the mechanism.

### Packed id-shard records (bygg, 2026-11-09)

Pass A's id-shard record went from three `u32` fields (12 bytes) to one
`u64` word whose field widths are sized per run (`IdCodec` in
`src/commands/altw/external/mod.rs`; planet: 26-bit local id, 18-bit blob
ordinal, 19-bit slot, closure bit), with a second word only for a slot
that does not fit. Planet: 12,435,459,911 records, zero escaped, id shards
149.2 GB -> 99.5 GB written in pass A and read back in stage 2.

`brokkr add-locations-to-ways --dataset planet --index-type external
--bench 1`, baseline `0182566d` as a `--commit HEAD` worktree build
(brokkr#105 applies to it), variant uncommitted `--force` on the main tree,
five cells back to back, untrimmed (sudo refused in the session), in run
order:

| Cell | Wall | Pass A | Stage 2 | Shard write rate | ID |
|---|---:|---:|---:|---:|---|
| packed 1 (after unrelated cells) | 301.2 s | 22.3 s | 147.2 s | 4.5 GB/s | unstored |
| 12-byte 1 | 336.2 s | 41.5 s | 163.8 s | 3.6 GB/s | `39608b9a` |
| packed 2 | 335.9 s | 43.1 s | 159.2 s | 2.3 GB/s | unstored |
| 12-byte 2 | 370.9 s | 65.6 s | 169.9 s | 2.3 GB/s | `a0259d8c` |
| packed 3 | 367.0 s | 54.9 s | 164.3 s | 1.8 GB/s | unstored |

(Shard write rate is bytes written over the pass A span.) Every cell ran
slower than the one before it, by about 17 s, whichever arm it was: the
run-history drift of the section above. Against the mean of its two
neighbours, packed 2 is -17.6 s wall (-5.0 %) and -10.5 s in pass A;
across all five, drift-corrected, packed is about -19 s wall and -13 s
pass A (-25 %), and every adjacent pair has the packed cell faster or
equal. Stage 2 also gains (one third fewer bytes to read, and it now reads
and decodes one shard at a time). Peak anon 11.67 against 11.35 GB in the
last pair (stage 4's peak, not shards). The wall figure carries the
worktree-build confound; pass A, bound by scratch writes, is the clean
part of the result.

### altw external pass A after a trim or after another cell (bygg, 2026-11-09, at `098c89cb`)

`brokkr add-locations-to-ways --dataset planet --index-type external
--bench 1`, six stored cells in one chain, each preceded by the previous
cell, no `sync`. "Trim" is `sudo fstrim -av` between the previous cell
and launch; "no trim" launches a few seconds after the previous cell
ends. Drive: WD_BLACK SN850X 4 TB, ext4 without `discard`, 55 % used.
`/proc/meminfo` read at the boundaries showed about 12 MB Dirty and zero
Writeback. Seconds (shard write is `s1a_id_shard_write_ms`, summed over
threads):

| Cell | Before it | Trimmed | Pass A | Shard write | Stage 2 | Wall | UUID |
|---|---|---:|---:|---:|---:|---:|---|
| 1 | trim | 13.6 GiB | 22.6 | 163 | 129.9 | 276.4 | `6bf4f504` |
| 2 | no trim | - | 41.5 | 474 | 160.4 | 327.4 | `a2261b63` |
| 3 | trim, a few s | 600 GiB | 23.2 | 246 | 139.7 | 284.3 | `3b0b7eac` |
| 4 | no trim | - | 36.1 | 419 | 143.3 | 313.7 | `8bfccb49` |
| 5 | no trim | - | 41.2 | 641 | 150.9 | 315.9 | `32bd503e` |
| 6 | trim, over 120 s | 828 GiB | 29.3 | 349 | 145.0 | 290.4 | `6b7b97c5` |

Every trimmed cell's pass A is faster than every untrimmed one's, and
cell 5 shows the slow state persists through an untrimmed predecessor.
Cell 3 is the strongest contrast: a trim of a few seconds took pass A
from 41.5 to 23.2 s, where a few seconds without a trim left it at 36-41
s. Stage 2 moves the same way but less, so not the whole wall delta is
pass A. What this does and does not establish:

- A large inherited kernel dirty-page backlog at launch is ruled out by
  the boundary readings. Dirty throttling during pass A and outstanding
  device-side work are not.
- The no-trim arms were not given a gap matched to the trim's duration,
  and no idle-only rest cell was run, so trim is not isolated from
  elapsed time; cell 6 combines a trim with a gap over two minutes and
  apportions nothing between them. The finding is a short-gap
  trim-associated recovery, from one ordered chain.
- Host counters cannot see inside the drive, so trim debt and
  trim-enabled cache or GC recovery are not separated.

No code change follows: the library cannot trim (FITRIM needs
`CAP_SYS_ADMIN` and acts on the whole filesystem) and `discard` is host
mount policy. Reducing scratch bytes does reduce the exposure (the
packed records above); the per-worker x 256 shard stream topology is
untested. Measurement consequence: trim before every altw external cell
of a matched A/B on this host, not only before the suite.

### Packed varint count (bygg, 2026-10-31)

Block construction counts every packed varint array (the dense id, lat, lon
and metadata arrays, way refs and coordinates, relation members) to check
parallel-array lengths. `e61e0ad7` made the count refuse a tenth byte above
1 by carrying a position within the varint through a per-byte loop, which
replaced a plain terminal-byte count. `brokkr read --dataset planet
--bench 1`, its parent `2ba3e170` then `e61e0ad7`, one pair, untrimmed:

| Mode | `2ba3e170` | `e61e0ad7` | Delta | UUIDs |
|---|---:|---:|---:|---|
| sequential | 524.9 s | 622.7 s | +18.6 % | `37e81b55` `9d17af61` |
| parallel | 28.7 s | 31.5 s | +9.7 % | `c7fd866a` `9dd0794c` |
| pipelined | 197.4 s | 196.1 s | -0.7 % | `2bac0419` `bb0de759` |
| blobreader | 562.2 s | 646.0 s | +14.9 % | `be0344af` `05ca943b` |

`brokkr sidecar --compare 37e81b55 9d17af61` shows the same single core and
the same bytes read: pure decode CPU. `cat --clean --dataset europe` on the
same pair is flat (158.7 s `c6513257` against 156.3 s `a822749f`): its
encode and compression hide the count.

The count now works eight bytes at a time (a mask and a popcount per word,
only the continuation run length carried between words; arrays holding a
run of nine continuation bytes take the exact per-byte scan), with the same
refusal. HEAD `14909999` as a stored `--commit` cell against the change
uncommitted on top of it (`--force`, not stored), alternating, two pairs:

| Mode | `14909999` | change | Delta (pair 1, pair 2) |
|---|---:|---:|---:|
| sequential | 597.6, 597.2 s | 528.9, 526.4 s | -11.5 %, -11.9 % |
| parallel | 31.1, 30.7 s | 28.8, 28.9 s | -7.4 %, -6.1 % |
| pipelined | 193.2, 192.6 s | 190.0, 190.7 s | -1.7 %, -1.0 % |
| blobreader | 635.2, 637.8 s | 567.2, 567.0 s | -10.7 %, -11.1 % |

`14909999` UUIDs: `fe3c0729` `302e9434` `4ecbe852` `a6d47169` (pair 1),
`f6a3283c` `acb37618` `f7c4555d` `6c041b12` (pair 2). The signs agree across
pairs, and the change brings sequential and blobreader back to the level of
`2ba3e170`. Pipelined read is bound elsewhere and does not move.

The whole read-path range, `ce6f20d9^` (`c0f67d9e`) against `307982d7`
(the one frame decoder) and HEAD `82be10a4`, alternating cells later the
same day:

| Mode | `c0f67d9e` | `307982d7` | `82be10a4` |
|---|---:|---:|---:|
| sequential | 524.4, 522.5 s | 604.7, 602.7 s | 534.9, 535.2 s |
| parallel | 28.6, 29.1 s | 31.4, 31.4 s | 29.1, 29.5 s |
| pipelined | 196.0, 195.6 s | 195.3, 195.8 s | 191.8, 191.1 s |
| blobreader | 562.4, 560.8 s | 642.7, 641.9 s | 574.9, 577.6 s |

The range's +15 % is the count above and nothing else: HEAD sits within
about 2.5 % of the range's start on every mode (sequential and blobreader
+2.2 to +2.6 %, both cells; pipelined -2 %), inside the noise floor. UUIDs
per run in mode order: `fa1a43ed` `cf9ee896` `aa12912c` `6104a00d` and
`c2a44d9a` `50eb615e` `4d6151f6` `dc6ccb4e` (`c0f67d9e`); `725c022a`
`a9baa55a` `0adb24b2` `02d7558c` and `6e9f2082` `1f946c48` `b6f51bdd`
`ddbaec65` (`307982d7`); `d52a1091` `455504be` `e0f24d30` `c432d7cc` and
`0a3ca1d6` `cbb3177c` `4738d079` `c70c136d` (`82be10a4`).

### External stage 1 worker cap (bygg, 2026-10-28)

Question: should external stage 1 pass A cap its worker count like stages
2 and 3 do? Uncapped it runs host parallelism minus 2, 30 workers on bygg,
each owning one 256 KiB writer per id bucket: 7,680 shard files and about
1.9 GiB of shard buffers. Arm A is `49a03ebb` (uncapped); arm B is
`fea271fd` (cap 8 applied before the descriptor request; a measurement
commit whose code was not kept). Planet, `--index-type external`,
`--bench 1`, alternating A then B, untrimmed. Seconds:

| Pair | Arm | UUID | Wall | Pass A | Stage 2 | Streaming |
|---|---|---|---:|---:|---:|---:|
| 1 | A uncapped | `2cf75884` | 546.7 | 114.6 | 210.9 | 212.4 |
| 1 | B cap 8 | `d05cddff` | 599.7 | 144.1 | 224.8 | 222.6 |
| 2 | A uncapped | `3ccbb592` | 708.0 | 226.0 | 238.9 | 234.9 |
| 2 | B cap 8 | `eed6408f` | 676.3 | 224.7 | 227.1 | 216.2 |
| 3 | A uncapped | `b302f622` | 728.0 | 225.0 | 248.1 | 246.5 |
| 3 | B cap 8 | `c83e69bd` | 701.3 | 221.2 | 244.0 | 227.7 |

Wall medians 708.0 (A) against 676.3 s (B), signs +, -, -. Pass A tied in
the slow-drive pairs and lost 26 % under the cap in the one fast-drive
pair, where at 8 workers each worker's serial pread, decode and emit chain
had less company to hide it (cumulative pread 461 s at 8 workers against
289 s at 30). Cumulative shard write time collapses with the cap (pair 2:
6,050 against 746 worker-seconds) because fewer workers sit in `write()`
at once; these are occupancy figures, not device service time. The cap's
wall wins in pairs 2 and 3 sit in stage 2 and streaming, opposite in sign
to pair 1 and smaller than the drift between consecutive cells.

Verdict: a cap of 8 did not show a reliable whole-command improvement, and
stage 1 stays uncapped on the CPU side; the descriptor cap still bounds it.
This does not show that 30 workers are free when the drive is slow, only
that capping did not pay. Decoupling file count and buffer RAM from the
worker count is the open alternative.

### Ordered pool: notify only parked workers, passthrough ahead of its result (bygg, 2026-11-08, against `141bb4c4`)

Two changes in `src/ordered_pool.rs`. The admission gate's `consumed` is an
atomic: `advance` stores it and takes the mutex and `notify_all`s only when
a worker is registered as parked, where it used to lock and notify on every
consumed result (std's futex condvar makes a wake syscall even with no
waiter). And `before_item(k)` runs once task `k - 1` is consumed, before
the wait for `k`'s result, so a passthrough interval (altw stage 4's node
prefix under `--keep-untagged-nodes`, extract's verified node blobs) is
copied while its next decode is produced.

Context from stored cells at post-`c2409c45` commits on bygg (altw
external `13668dda`, `467a50a9`; tags-filter `b8491cfd`; time-filter
`bb2517a3`; getparents `dd453b3f`; repack `723e4963`; multi-extract
`21d32a8f`; extract smart `05bbc210`): `ordered_pool_window_wait_ns` is 0
in every one. The window never bound in those runs, so stage 1 workers
never parked behind a slow head and the herd on a full window did not
occur; altw stage 4 is consumer-bound on result send.

Planet reindexed input, `--bench 1`, variant uncommitted (`--force`,
unstored), baseline clean `141bb4c4`. Seconds:

| Cell | Arm | UUID | Wall | Pass A | Stage 2 | Stage 4 |
|---|---|---|---:|---:|---:|---:|
| altw external 1 | variant | `1e24412d` | 364.2 | 37.0 | 177.9 | 140.7 |
| altw external 1 | baseline | `ce032961` | 379.4 | 59.2 | 172.3 | 140.2 |
| altw external 2 | baseline | `eb94af17` | 415.3 | 83.3 | 180.2 | 144.1 |
| altw external 2 | variant | sidecar `dirty` at 21:30 | 400.1 | 63.7 | 183.9 | 144.9 |
| tags-filter w/highway=primary 1 | variant | `16739974` | 52.9 | | | |
| tags-filter 1 | baseline | `9406cf2a` | 52.6 | | | |
| tags-filter 2 | baseline | `4c79e049` | 57.8 | | | |
| tags-filter 2 | variant | `2ab0cbf4` | 52.5 | | | |

Stage 4 is flat (140.7/144.9 against 140.2/144.1 s); the altw wall gap is
pass A, which swings with run history ("altw external pass A swings with
run history" above), not with this change. Peak anon 10.22 against 10.26
GB. tags-filter is flat apart from one slow baseline cell. Verdict: no
measurable cost or gain on the default paths. The `--keep-untagged-nodes`
case is measured in the next entry.

### Eager passthrough under `--keep-untagged-nodes` (bygg, 2026-11-09, at `b58e8ef3`)

The case the eager `before_item` above targets: altw external with
`--keep-untagged-nodes`, where stage 4 copies 33,287 node and relation
blobs (62.6 GB, `s4_passthrough_pread_ms` 51.4 s) between 17,529 decoded
way blobs. Arms: the tree's eager hook against an uncommitted lazy
variant that calls `before_item(k)` only once task `k`'s result is
popped, before its error is unwrapped (the pre-`e91e5f11` placement).
Both arms carry the read-ahead advice change of the next entry. Planet
reindexed input, `--bench 1`, `--force` (unstored), `sudo fstrim -av`
before every cell, order eager, lazy, lazy, eager. Seconds:

| Cell | Arm | Wall | Pass A | Stage 2 | Streaming (stages 3+4) | `WAIT_S4_SEND` total |
|---|---|---:|---:|---:|---:|---:|
| 1 | eager | 302.5 | 22.3 | 130.0 | 142.8 | 135.1 |
| 2 | lazy | 302.6 | 22.1 | 130.9 | 142.3 | 136.8 |
| 3 | lazy | 324.4 | 28.3 | 144.5 | 145.1 | 139.4 |
| 4 | eager | 297.5 | 22.4 | 129.1 | 138.7 | 133.3 |

`EXTJOIN_STAGE4` shares its START/END with `EXTJOIN_STAGE3`, so the
streaming span is stage 3's: stage 4's workers spend about 135 s of it
summed in `WAIT_S4_SEND`, and the consumer's own write time
(`s4_consumer_write_ms` about 64 s in both arms) fits inside the span
either way. Cell 3 is slow in every phase, pass A and stage 2 included,
which no stage 4 change reaches: drive state. Verdict: no measurable
effect; the passthrough copy is hidden behind stage 3 whether it runs
ahead of the next result or not. The eager hook stays, for its error
ordering and liveness contract, not for speed.

### Read-ahead advice after the credit (bygg, 2026-11-09, at `b58e8ef3`)

`ClassifyConsumer::on_advance` issues the sliding `POSIX_FADV_WILLNEED`
on the ordered pool's consumer thread. It ran before the consumed
result's credit was published, so a slow advice call held back workers
parked at the window. It now runs after `gate.advance`. Every task newly
admitted by a credit lies inside the byte window an earlier call already
advised, unless the schedule jumps more than `WILLNEED_WINDOW_BYTES`
between adjacent tasks, and advice issued in the instant before
admission gives such a task no lead either way. New counter
`fadvise_willneed_wait_ns`: the time the advising thread spends inside
the syscall (not the read-ahead I/O, not reclaim alone), emitted once per
advising pass. Variant (advice before credit, the old order) and tree
both uncommitted with the counter, `--force`, `cat --clean`. Seconds;
window and advice as % of wall:

| Dataset | Arm | Wall | `ordered_pool_window` | `fadvise_willneed` |
|---|---|---:|---:|---:|
| planet reindexed | after credit | 142.7 | 0 | 1.96 (1.4 %) |
| planet reindexed | before credit | 142.1 | 0 | 1.99 (1.4 %) |
| germany | before credit (first after planet, cold) | 8.9 | 1.07 (12 %) | 0.21 |
| germany | after credit | 7.0 | 0.91 (13 %) | 0.03 |
| germany | before credit | 7.0 | 1.01 (15 %) | 0.03 |
| germany | after credit | 7.1 | | |

An earlier planet cell of the tree arm before the counter was wired
measured 142.8 s. Verdict: no difference outside noise. The window binds
on germany (12 to 15 % of wall) in both arms, while warm advice costs
30 ms in total there, so what binds the window is the head blob's own
work, not the advice. On planet the consumer spends 2 s of a 143 s wall
in advice, 1.4 %, with `ordered_pool_result_recv` at 90 %: the consumer
placement is not a cost, and neither a worker-side cursor nor a dedicated
advisor thread has a cost to recover. Whether the advice pays at all on
the ordered pools is not decided by these cells.

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
defaults to zlib:1.

### Planet (87.7 GB indexed, 11.6B elements, plantasjen)

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways /
14,124,889 relations / 12,435,459,911 way refs.

### Memory

Peak anon 3.3 GB (commit `cb99106`). Single shared `IdSetDense` with
`AtomicU8::fetch_or` for concurrent pass 1 writes (~1.5 GB node bitset
+ rank index, ~200 MB way bitset + rank, ~20 MB relation bitset).
Zero temp disk. Renumber ran with glibc capped at two arenas until
2026-11; without the cap the planet peak is 3.32 GB against 3.16 GB with
it, at the same wall (see "The CLI no longer caps malloc arenas").

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
Europe and Planet bbox `-25.0,34.0,45.0,72.0` (full-continent). Europe's
is its configured dataset bbox; planet's dataset bbox in `brokkr.toml` is a
Berlin box chosen for `brokkr verify` cost, so planet extract cells that
want the continent box pass `--bbox` explicitly.

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

### Parent relations, integer bbox edges and plain nodes (bygg, 2026-11-03, against `4a3ba718`)

The change that brought extract's membership to osmium's: complete-ways and
smart close the relation set over parent relations (member-to-parent edges
collected in the relation classify phase, sorted, walked from the matched
set), every bbox membership test compares integers against one quantized box
(simple's node classify included, where the f64 product comparison ran per
node), and the sorted columnar classifier also walks plain Node messages.
Planet, Europe box `-25.0,34.0,45.0,72.0`, `--bench 1` main-tree cells,
interleaved, after `fstrim`; baseline = `4a3ba718` stored, variant = the same
tree with the change, `--force`:

| Strategy | baseline | variant | baseline peak | variant peak |
|----------|----------|---------|---------------|--------------|
| smart | 132.99 s (`d6c01405`), 132.56 s (`a49fcc12`) | 133.15 s (`dea99869`), 132.42 s (`39c9957b`) | 8.14 / 8.11 GB | 8.10 / 8.13 GB |
| complete | 118.90 s (`f7cb91a2`) | 119.05 s (`ccaa5096`) | 4.92 GB | 4.93 GB |
| simple | 108.37 s (`01fcae89`) | 108.15 s (`0a37fc61`) | - | - |

All within noise. The parent closure (`PASS1_RELATION_PARENTS`) is 209 ms
over 1 042 815 edges and adds 78 408 relations; the relation classify phase
that collects the edges stays at 0.94 s. Variant UUIDs are dirty sidecar
rows (`brokkr sidecar <uuid>`), not `results` rows.

### Polygon membership on osmium's banded integer test (bygg, 2026-11-03, against `c4ed2ad8`)

The polygon predicate moved from an f64 ray cast over every vertex per node
to osmium's integer test over the segments of the node's latitude band.
brokkr runs extract and multi-extract with bbox regions only, so these are
direct runs of the two release binaries, interleaved, wall clock, not stored
cells; bbox extracts do not reach the changed code. Boundaries exported by
`osmium export` from the same input:

| Input, region (vertices), strategy | `c4ed2ad8` | variant |
|------------------------------------|------------|---------|
| denmark-20260220, Aarhus Kommune (9694), simple, 4 pairs | 1.26 s median (1.26 to 1.28) | 0.35 s median (0.35 to 0.36) |
| germany-20260224, Berlin (6017), complete, 2 pairs | 5.70 s warm, 11.10 s cold first run | 3.28 s, 3.31 s |

Prepared segments and bands are built once per region; the per-node cost is
the band's segments instead of every vertex, so the gain grows with the
polygon's vertex count.

### ID sets that hold only what the writer does not already have (bygg, 2026-11-08, against `0973c352`)

complete and smart keep the node refs of matched ways only where they are
outside the region set (the writer includes the union and tests the region
set first, so a ref in both bought nothing); smart's relation-driven extras
skip matched ways and already-included nodes, and its pass-1 relation
classify sends member IDs per blob instead of accumulating an `IdSet` per
worker. Planet (`planet-20260223-reindexed`), Europe box
`-25,34,45,72`, `--bench 1` main-tree cells; baseline = `0973c352` stored,
variant = the uncommitted change, `--force` (dirty sidecar rows):

| Strategy | baseline wall | variant wall | baseline peak anon | variant peak anon |
|----------|---------------|--------------|--------------------|-------------------|
| complete | 116.98 s (`80089cbc`) | 116.14 s (`8c487309`) | 4.44 GB | 2.76 GB |
| smart | 129.21 s (`05bbc210`) | 128.73 s (`15c17254`) | 8.06 GB | 2.87 GB |
| simple, node set dropped before the way write | 106.81 s (`38954d07`) | 107.51 s (`6d5424f1`) | 2.57 GB | 2.60 GB |

complete's way classify fell from 16.1 s to 12.5 s (it sets far fewer IDs).
smart's 8 GB was the relation classify phase: 32 workers each promoting
dense chunks for dispersed member IDs; per-blob send removes it, and the
phase falls from 0.85 s to 0.36 s. The simple row is a refusal: moving the
relation classify ahead of the way write so the node set could be dropped
there freed live memory but moved no peak, since the freed chunks stay in
the arenas and the way write's worker-side buffers do not reuse them; it was
not landed (the reason sits at the site in `simple.rs`).

### WILLNEED on simple's node and way classify (bygg, 2026-11-08, at `e91e5f11`)

`8928de18` moved simple's node and way classify onto the unordered driver
(`run_unordered_per_blob`), which gave them the dispatcher's sliding
`WILLNEED` advice. This is a HEAD policy assessment, not the historical
`8928de18^` against `8928de18` pair: that commit also changed the classify
descriptor, the worker buffers and the counters, all reshaped since, so its
pair could not be acted on at HEAD. Those other parts stay unmeasured in
isolation. Baseline = HEAD stored; variant = uncommitted, the driver building
its `WillneedPrefetch` from an empty schedule (no ranges, no advice, no
range construction), `--force`. In an `extract --simple` run only simple's
two classify pools reach that driver; the relation classify keeps its own
advice in both arms. Box `-25,34,45,72` on both datasets, `--bench 1`,
interleaved, each cell preceded by an extract cell of the same dataset; no
trim before the suite.

| Dataset | order | with WILLNEED | without |
|---------|-------|---------------|---------|
| planet-20260223-reindexed | A B A A B, first A after a tags-filter | 109.76 s (`ac10b032`, warm-up), 108.71 s (`2bc9cc4a`), 108.14 s (`ecee41b7`) | 108.59 s (`45e8afe4`), 108.12 s (`56b95ea5`) |
| europe indexed | A B B A A B, first A after the planet cells | 53.07 s (`887fe55d`, warm-up), 44.63 s (`5d94493b`), 43.35 s (`dfe4ec94`) | 46.21 s (`08411270`), 43.97 s (`0171e902`), 43.70 s (`6e4d4afe`) |

`ac10b032` was stored and then lost from `results.db` to a stash; its wall
is from the run output. Planet phases match to within 1 % (node classify
16.59 against 16.44 s, way classify 8.05 against 8.21 s, `ecee41b7` against
`56b95ea5`). On europe the classify phases run slightly longer without the
advice (node 5.67 against 5.81 s, way 3.08 against 3.26 s, `dfe4ec94`
against `6e4d4afe`), about 0.3 s in a 43 s wall. Disk read differs more
than wall: the arm with the advice read 3.7 against 2.6 GB in node classify
and 9.7 against 9.4 GB in way classify, but its schedule scan also read 3.3
against 2.4 GB, so the gap tracks what the predecessor cell left resident
as much as the advice. Verdict: no demonstrated benefit or cost on either dataset; the
advice stays. A neutral result is not proof of equivalence: two or three
cells per arm cannot resolve a difference inside the planet band.

## Multi-extract

Single-pass simple strategy on sorted input: read PBF once, classify each
element against N regions, compress and frame each region's blocks in the
pool's tasks, write the frames to N sync-mode PbfWriters
(sync-mode writers over the staged files; publication syncs each output
after the final flush, once `MULTI_REL_WRITE_END` and every other phase
marker has ended, between `PUBLISH_START` and `PUBLISH_END`; the `writer_sync_all_ns` and
`writer_sync_all_calls` counters attribute that cost). The rows below
predate the per-output sync and the 256 KiB per-output buffer. 3-phase
barrier (nodes → ways → relations), with per-region `BlockBuilder`s and,
since the shared region membership below, one membership structure per
kind (the rows in this table predate it and used one `IdSet` per region
and kind). 5 disjoint longitude strips per configured bench.

### Shared region membership (bygg, baseline `4109d1c4`, 2026-11-08)

One `extract/membership.rs` `Membership` per kind (pages of 4096 IDs, a
union of members with a palette of region masks, a per-region mutable form
for revisited pages) replaced N `IdSet`s per kind. Before it, the five node
sets were 406 dense 4 MiB chunks each (8.5 GB, essentially the whole peak):
OSM IDs are chronological, so every region's nodes spread over the whole
ID history, and even 1M members over 400 chunks passes the 1024-member
sparse cap. Counted with temporary counters at baseline, item config: node
members per region 0.38e9 / 0.90e9 / 1.11e9 / 0.67e9 / 0.36e9 over 13.6e9
ID positions; way sets 5 x 45 chunks for 0.42e9 members.

Planet `planet-20260223-reindexed`, `--simple`, `--bench 1` cells,
interleaved. A is clean `4109d1c4` (stored); B is the uncommitted variant
on the same base (`--force`, numbers from `brokkr sidecar dirty`).

| Config | Arm | Wall | Peak anon | Node classify | Way classify | UUID |
|---|---|---:|---:|---:|---:|---|
| `--bbox=-10,35,32,72` | B1 | 118.8 s | 3.93 GB | 20.1 s | 17.4 s | (forced) |
| `--bbox=-10,35,32,72` | A1 | 121.5 s | 9.80 GB | 18.3 s | 20.9 s | `a89f3c1e` |
| `--bbox=-10,35,32,72` | B2 | 119.6 s | 3.94 GB | 20.2 s | 17.6 s | (forced) |
| default (dataset bbox) | B | 68.4 s | 0.79 GB | 16.8 s | 13.2 s | (forced) |
| default (dataset bbox) | A | 88.3 s | 2.01 GB | 17.2 s | 25.0 s | `21d32a8f` |

Item config: peak anon -60 %, wall -2 % (noise). Node classify pays
about 1.8 s for worker-side page encoding; way classify gains about 3 s
from one lookup per ref in place of up to N. Default config: -61 % peak
anon and -23 % wall: way classify halves and node write drops 29.4 s ->
23.3 s, the N small-but-dense sets having been cache-hostile lookups.
An earlier variant re-encoded every Incidence page at freeze (5.5 s
single-core at the end of node classify); freeze now re-chooses only
pages changed after their last encoding (52 ms).

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

### Compression inside the pool's tasks (bygg, 2026-10-31)

The write phases above were serial compression: every region's blocks were
compressed on the one consumer thread by its sync writer, so they ran at
2-3 user cores while the decode workers parked on result send (stored cell
`831424c0`, commit `179f1b7e`: `ordered_pool_result_send` 17,260
thread-seconds, `writer_compress_ns` 608 s against 613.6 s of write phases).
The workers now compress and frame each region's blocks in the task and the
consumer only copies frames into the writers. Output bytes are unchanged.

Planet, 5 strips `--bbox=-10.0,35.0,32.0,72.0`, simple, `--bench 1`,
interleaved baseline / variant / variant / baseline, baseline `d653c2b7`
(`--commit`), variant the same tree plus the change, measured uncommitted
with `--force`:

| Cell | Wall | NODE_WRITE | WAY_WRITE | REL_WRITE | Peak anon |
|---|---:|---:|---:|---:|---:|
| baseline `2185e803` | 675.2 s | 362.3 s | 257.3 s | 9.2 s | 9.86 GB |
| variant (forced) | 128.0 s | 52.2 s | 28.9 s | 0.9 s | 9.93 GB |
| variant (forced) | 128.7 s | | | | |
| baseline `ab191862` | 675.4 s | | | | |

The second pair's phase split was not re-read; its walls agree with the
first pair's to under 1 %.

Classify phases are unchanged (node 20.9 s, way 17.7 s). In the variant
`ordered_pool_result_send` is 0 and the consumer waits on the workers
instead (`ordered_pool_result_recv` 70.7 s): the write phases are now
decode, classify and compress bound across the pool.

Counters emitted (UUID `c1ff6ec9` values):
- `multi_extract_region_count=5`
- `multi_extract_node_blobs`, `multi_extract_way_blobs`,
  `multi_extract_relation_blobs` (schedule sizes)
- `multi_extract_nodes_written`, `multi_extract_ways_written`,
  `multi_extract_relations_written` (cross-region totals)

## Coordinate scaling (bygg, 2026-10-28)

`71fd420b` below is the measurement build of the commit that landed these
changes: the same non-test code, with this record and one more unit test
added when the commit was amended, so the hash in `results.db` is not on
`main`.

### Columnar granularity-100 shortcut

The columnar dense-node decode (`DenseNodeColumns::decode_append`, used by
`extract --strategy smart` and multi-extract node classification) once had a
granularity-100 shortcut, `cumulative + offset / 100`, that skipped the
per-node multiply and divide. It was added on an estimate (about 8 cycles x
4B nodes, about 10 s at planet), never measured, and it wrapped at a
different point from the element accessors, so on a block whose true
nanodegree value overflows `i64` it classified a node at a different
coordinate from every other reader. Removing it was measured as matched,
interleaved `--bench 1` cells on planet, `64e478f8` (with) against
`71fd420b` (without; the same build also carries the exact node-scanner
scaling below, which these commands do not run):

| Command | With (s) | Without (s) | Median delta |
|---|---|---|---:|
| `extract --strategy smart --bbox=-25.0,34.0,45.0,72.0` | 139.3 `fcd813b1`, 140.3 `61ae1b01`, 141.2 `f73d8063` | 138.3 `2275347f`, 140.9 `c5c4c31f`, 140.9 `069114e2` | +0.4 % |
| `multi-extract --regions 5 --strategy smart --bbox=-10.0,35.0,32.0,72.0` | 481.5 `c207a1e9`, 487.5 `a0e311f1`, 486.7 `7a65cd87` | 482.9 `71901454`, 487.8 `167d427e`, 487.4 `9137d2c6` | +0.1 % |

Node classification is the phase the shortcut lived in, and it did not
move: `PASS1_NODE_CLASSIFY` 21.76 s against 21.62 s (`61ae1b01` against
`c5c4c31f`), about 19 s on both sides in multi-extract. The phase reads
about 56 GB and runs about 25 cores; it is bound by decompression and I/O,
not by the scaling arithmetic. The shortcut was deleted.

### Exact node-scanner scaling

The node scanner (`scan/node.rs`, used by add-locations-to-ways and the
apply-changes `--locations-on-ways` extractors) scales with checked `i64`
arithmetic and falls back to exact `i128` only when an intermediate
overflows, so the hot path keeps its `i64` multiply and divide. Matched,
interleaved `--bench 1` cells on planet, `64e478f8` against `71fd420b`:

| Command | Before (s) | After (s) | Median delta |
|---|---|---|---:|
| `add-locations-to-ways --index-type external` | 735.6 `a8fe2f08`, 707.8 `bec9aaf8`, 675.1 `ba12609f` | 669.5 `20585f4b`, 691.2 `73adb2d8`, 698.5 `9874e28c` | -2.3 % |
| `apply-changes --locations-on-ways`, `altw-keep-untagged` base, OSC 4913 | 335.6 `c5ff6ab4`, 307.3 `f95e13b2`, 307.5 `558d3e1e` | 311.5 `029a1e71`, 305.5 `b40e1b3c`, 310.8 `49d568d7` | +1.1 % |

Both inside noise, with mixed signs across pairs. The altw pair that
overlapped a manual disk cleanup (`974d616a` / `10f71ab4`) was discarded and
rerun as `ba12609f` / `9874e28c`. The altw node stage reads about 199 GB at
under 3 cores, so it is I/O bound. No fstrim preceded these cells.

### Scanner granularity read and the join-time domain check

The node scanner reads `PrimitiveBlock.granularity` as any `int32`, like
every reader, instead of bounding it to `1..=1e9` (a per-block change), and
apply-changes `--locations-on-ways` refuses a kept base node outside the
geographic domain (one branch on the kept-node path of the extraction sink,
not per scanned node). Matched, interleaved `--bench 1` cells on bygg,
2026-10-28, after a fresh `fstrim`: baseline is a clean `38f6ee8c`; the
variant is the same tree with the change as uncommitted edits, run with
`--force`, so its cells are not in results.db and its numbers come from
the run output and `brokkr sidecar dirty`.

| Arm | Wall (s), in run order | Median | `merge_streaming_coord_extract_ns` summed over workers (s), median |
|---|---|---:|---:|
| Baseline `38f6ee8c` | 305.6 `f1973516`, 301.4 `bc1e7026`, 302.4 `8995dd68` | 302.4 | 235.7 |
| Variant (forced) | 295.6, 297.9, 299.8 | 297.9 | 237.9 |

Wall -1.5 % (variant faster in all three pairs), extraction time +0.9 %:
both inside noise. add-locations-to-ways was not re-measured: its per-node
code is untouched, and the granularity read runs once per block.

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

### Direct and member ways kept apart (bygg, 2026-11-08, against `0973c352`)

The included-ways set held every direct match a second time; it now holds
only the ways the relation closure pulled in that are not direct matches,
and the way-dependency walk, which only those ways need, is skipped when
there are none (every expression without a matched relation). Planet
(`planet-20260223-reindexed`), `w/highway=primary`, `--bench 1` main-tree
cells in the order A B B A; baseline = `0973c352` stored, variant =
uncommitted, `--force` (dirty sidecar rows):

| Arm | Wall | Peak anon | UUID |
|-----|-----:|----------:|------|
| baseline | 69.05 s | 2.49 GB | `f60408df` |
| variant | 55.93 s | 2.27 GB | `0b19b3c4` |
| variant | 56.67 s | 2.31 GB | `6e9a4f10` |
| baseline | 71.37 s | 2.44 GB | `097ae97a` |

The wall gain is the removed walk (its schedule scan plus about 9.6 s of way
blobs read for nothing); the peak gain is the duplicate way set. Relation
expressions still walk the way blobs, and their peak moves by the size of
their direct-way set only.

### Single pass across the fused engine's transform seam (bygg, 2026-11-09, `2529f73f` vs `c0f67d9e`)

`c0f67d9e` gave the fused engine a transform trait so digest could see each
blob before its decode: the decode-worker closure now hands the transform a
decode closure, and the per-thread string-table and group scratch borrows
end before the transform runs rather than around it. `tags-filter -R` goes
through that engine at both commits (block-only adapter, indexdata parsing
unchanged). Planet (`planet-20260223-reindexed`), `-R w/highway=primary`,
`--bench 1`, both arms as `--commit` worktrees, order A B B A:

| Arm | Wall | UUID |
|-----|-----:|------|
| `2529f73f` | 31.58 s | `4c2ea16c` |
| `c0f67d9e` | 31.54 s | `ff665e30` |
| `c0f67d9e` | 31.52 s | `ab6ceb19` |
| `2529f73f` | 31.60 s | `c44ad99a` |

Medians 31.59 s against 31.53 s (-0.2 %), both pairs the same sign; the one
scan phase runs about 11 cores in both arms. The filter skips 33,287 data
blobs on tagdata, so this screens the seam on the blobs the expression
decodes, not on every fused workload. A regression screen, not a bound;
no re-measure without a change to the decode-worker closure.

## Merge-changes

`pbfhogg merge-changes` squashes N OSC (gzip + XML) inputs into one OSC
output. Two production code paths on the same lexical readers:
**streaming** (`write_streaming`, the default) and **simplify**
(`--simplify`, collects every change as rendered XML, dedupes by a
parallel sort and splices the survivors' bytes).

### Run directory only where a merge may spill (bygg, 2026-11-01)

The streaming merge takes an optional scratch, given only for several
inputs at any `-j` but 1; the coordinator's spill site unwraps it once per
spilled input. Matched interleaved `--bench 1` cells on bygg, 7-OSC
`4914..4920`, default mode: baseline `6a9807d9` built in brokkr's
`--commit` worktree (stored), variant `6a9807d9` + change (`--force`,
unstored).

| Cell | Baseline | Variant |
|---|---|---|
| merge-changes 7-OSC (default), three pairs | 3.667, 3.726, 3.721 s (`3ea1430d`, `497ad8ea`, `16d34181`) | 3.693, 3.716, 3.695 s (median -0.7 %, noise) |

The phase split of the third pair is identical (parallel emit 3467 ms
against 3440 ms, drain 249 ms on both). One-input, `--simplify` and `-j 1`
runs lose only the run directory's creation and were not measured: brokkr
passes `--tmp-dir` to every merge-changes, which those command lines now
refuse.

### OSC tags-filter takes -j (bygg, 2026-11-02)

`tags_filter_osc` takes a `Parallelism`: `Fixed(n)` runs the batch workers
on a pool of `n` built for the call, `Auto` on the host executor as before,
so the default path is the same code. Planet 4913 `w/highway=primary`,
`--bench 1 --force` cells, both arms built in the main tree on `befcab8e`
(baseline: the two changed source files checked out from HEAD). Baseline
2.440, 2.429, 2.435, 2.418 s; variant 2.599, 2.514, 2.411, 2.419 s. The
two slow variant cells were each the first after a rebuild, as was the
baseline's 2.440 s; warm cells agree within 1 %. Variant peak anon 135 MB,
2.6 cores. Output at `-j 1`, `-j 8` and the default is byte-identical
(sha256). brokkr refuses `-j` on this shape, so no `-j` sweep was measured.

### Simplify and OSC tags-filter on the batch readers (bygg, 2026-10-31)

`--simplify` used to parse each input on one thread into owned elements,
concatenate them, dedupe them serially through a `BTreeMap`, then re-emit
the survivors. It now runs on the streaming path's readers: workers render
each batch to plain XML and record per element its kind, ID, action,
version claim and byte range; a parallel sort of `(kind, id, position)`
replaces the map; survivors' bytes are spliced into the same per-worker
chunks, so the output bytes are unchanged (a differential test against the
owned path holds them, gzip members included). `tags-filter --input-kind
osc` moved onto the same readers, its workers rendering kept elements into
nine (action, kind) groups.

Matched interleaved `--bench 1` cells on bygg. Baseline `44c62853`: the
simplify cells were built in brokkr's `--commit` worktree, the others in
the main tree (stash, `--bench 1`, stored); variant `44c62853` + change
(`--force`, unstored). Spans from `--durations` START/END pairs.

| Cell | Baseline | Variant |
|---|---|---|
| merge-changes `--simplify` 7-OSC `4914..4920` | 20.9 s, 18.3 s; peak anon 22.5 GB (`0df5c0ad`, `2b1d6966`) | 5.44, 5.49, 5.41, 5.40 s; peak anon 12.0 GB (11.8 GB before element ranges widened to 64-bit offsets) |
| merge-changes 7-OSC (default), three pairs | 3.619, 3.657, 3.625 s (`0028f8ee`, `dc3aebc0`, `1849bf51`) | 3.636, 3.704, 3.656 s (median +0.9 %, noise) |
| `tags-filter --input-kind osc` 4913 `w/highway=primary` | 5.31 s, peak anon 244 MB (`68a6c81f`) | 2.46, 2.46 s; peak anon 131 MB |

Variant simplify spans: parse and render 3.18 s (was 6.9 s of owned parse
plus a 0.6-1.8 s concatenation), dedupe 0.23 s (was 4.3 s), emit 1.59 s,
drain 0.29 s. The peak is the rendered XML of all 26.3 M input changes
plus a 24-byte sort key and a 40-byte record per change, then the deflated
chunks during emit.

### Streaming path on lexical batches with spill (bygg, 2026-10-30)

The streaming path no longer makes one whole input the unit of work. One
reader thread per input (capped by the worker count, started in input
order) drives the OSC driver and records each element's raw tags into
batches of `MERGE_CHANGES_BATCH_BYTES`; pool workers replay a batch into
the owned sink, re-emit XML and gzip it as its own member; one
coordinator writes each member as soon as it is next in order for its
input, the head input's to the output and later inputs' to a spill file
in the run scratch, copied into the output when that input becomes the
head. A byte ledger (`MERGE_CHANGES_INFLIGHT_BYTES`) bounds admitted
batches. The single-input run takes the same path and is no longer
serial.

| Cell | Baseline (commit `627883d7`) | This change (`418739bd` + change, `--force`) |
|---|---|---|
| planet 7-OSC `--osc-range 4914..4920` | 30.0 s, peak anon 895 MB (`f24c29af`) | **9.9 s, peak anon 286 MB** (an earlier cell of the same design before review fixes: 10.0 s, 304 MB) |
| planet 1-OSC `--osc-seq 4913` | 24.0 s, peak anon 1.6 MB (`bf6db512`) | **2.74 s, peak anon 53 MB** |

Same change counts as the baselines (26,261,540 and 4,305,714). The
7-OSC output is 965.1 MB against 962 MB (one gzip member per batch).
Spill: about 787 MB written for inputs 2-7 and copied back. The readers
spent 15.6 s summed waiting for credit
(`merge_changes_credit_wait_ns`), so the 64 MiB budget bound at planet;
since `179f1b7e` the readers never wait on it (below). The 1-OSC peak rises
from a single-threaded stream to the batch working set, accepted.

### One deflate per batch, no owned elements (bygg, 2026-10-31)

At `82be10a4` the workers were the bound (30.7 cores, readers parked
16.8 s summed on the credit ledger), but not because of the XML: every
quick-xml event went straight into a `GzEncoder`, so deflate ran once
per tiny write. `179f1b7e` writes each batch's XML to a plain buffer and
deflates it in one call, and replays batches into a byte sink that
writes the owned writers' bytes without building owned elements. The
borrowed sink alone, measured first as an uncommitted cell, moved the
7-OSC wall only from 9.9 to 9.35 s; the deflate change took it to 3.7 s.

| Cell | `82be10a4` | `179f1b7e` |
|---|---:|---:|
| 7-OSC `--osc-range 4914..4920` | 9.93 s (`c8b6af17`) | 3.73, 3.70 s (`ba9e4ad7`, `444fd7fa`) |
| 1-OSC `--osc-seq 4913` | 2.61 s (`b0175d10`) | 2.40 s (`bd19099c`) |
| 7-OSC `--simplify` | 34.2 s (`2f49e9c1`) | 17.7 s (`3ae86540`) |
| `tags-filter --input-kind osc`, 4913 | 6.25 s (`e6749613`) | 5.24 s (`c1e30598`) |

The 7-OSC run is now reader bound: 19 cores, no credit wait, peak anon
123 MB from 295 MB. The plain-XML output of the 7-OSC merge is
byte-identical to `82be10a4`'s; the gzip members differ in deflate
chunking only.

### OSC driver enforces the element hierarchy (bygg, 2026-10-30)

The OSC driver now tracks a fixed-depth level (root, section, object,
leaf) and refuses misplaced elements; the `End` arm pops by level instead
of classifying the closing name. Matched interleaved `--bench 1` cells on
bygg, baseline clean `b1f522f7` (stored), variant `b1f522f7` + change
(`--force`, unstored):

| Cell | Baseline | Variant |
|---|---|---|
| merge-changes planet 7-OSC, pair 1 | 9.886 s (`05f37130`) | 9.921 s |
| merge-changes planet 7-OSC, pair 2 | 9.900 s (`6d76714f`) | 9.914 s |
| tags-filter `--input-kind osc` planet 4913 | 6.205 s (`9e459dcd`) | 6.008 s |

All deltas are inside noise (+0.4 %, +0.1 %, -3.2 %); the hierarchy check
costs nothing measurable on the reader-thread-bound merge path.

The sections below record the earlier whole-input shapes.

### Streaming-path planet 7-OSC: 5.0× speedup (commit `99057fa`, plantasjen, superseded)

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

A later bounded window over whole-input members still held every
admitted input's compressed output in memory, so with fewer inputs than
threads peak memory was about the output size (895 MB in `f24c29af`).

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

At `abd1d9e` the simplify wall was 19 s slower than the streaming wall
on the same input: the dedupe phase (6.9 s) plus a less efficient parse
fan-out (workers parsed to a separate owned buffer in simplify, vs the
streaming path's fused parse and emit of that time).

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

## Library capability accounting

### Call guards on per-call methods (bygg, 2026-10-29)

The invocation's capability accounting adds success-path work on per-call
public methods: a call guard (one compare-and-swap to admit, one
`fetch_sub` to retire, on a padded word) per `BlobReader::next` and per
`PbfWriter` write method, plus one census-mutex lock at mint and retire per
block a block API hands its caller. Gated by `brokkr write` (public
`BlobReader` iteration and decode, rebuilt into blocks written through
`PbfWriter::write_primitive_block`), denmark `--variant indexed --bench 1
--compressions none`, cells interleaved. Baseline `--commit 1f2919e4`
(stored); variant the capability tree on top of `1f2919e4`, measured
uncommitted with `--force` (forced cells are not stored, so they carry no
UUID).

| Writer | Baseline pair 1 | Variant pair 1 | Baseline pair 2 | Variant pair 2 | Medians |
|---|---|---|---|---|---|
| sync | 6702 ms (`95ba54f2`) | 6499 ms | 6670 ms (`40463eef`) | 6480 ms | 6686 vs 6490 ms |
| pipelined | 5829 ms (`f621aa90`) | 5846 ms | 5792 ms (`3292831a`) | 5713 ms | 5810 vs 5780 ms |

No cost: the sync arm is faster in both pairs, the pipelined arm within
noise with mixed signs. No brokkr command issues blocks in bulk (`brokkr
read --mode pipelined` runs `for_each_pipelined`, which lends elements and
issues nothing; `build-geocode-index` iterates relation blocks only, through
an internal route), so the census lock per issued block has no bulk
measurement; it is one uncontended mutex per block, beside a decode.

### Fallible admission (bygg, 2026-10-30)

Sealing makes admission fallible: the guard's compare-and-swap now also
requires the Open state (a mask and a branch on the word it already
loaded), and `BlobReader::next` checks its walk's failure latch before it
admits. The call sink's sealing path keeps no in-flight count: the word
orders every emission before the seal, so no brokkr CLI command gains
per-emission work. Same gate as above: `brokkr write`, denmark `--variant
indexed --bench 1 --compressions none`, cells interleaved. Baseline
`--commit 62e18608` (stored); variant the sealing tree on top of it,
measured uncommitted with `--force` (not stored, no UUID).

| Writer | Baseline pair 1 | Variant pair 1 | Baseline pair 2 | Variant pair 2 | Medians |
|---|---|---|---|---|---|
| sync | 6415 ms (`66a370df`) | 6468 ms | 6412 ms (`5294bccd`) | 6416 ms | 6414 vs 6442 ms |
| pipelined | 5724 ms (`e0935539`) | 5753 ms | 5737 ms (`3c12b82f`) | 5679 ms | 5731 vs 5716 ms |

No cost: both arms within half a percent, the pipelined arm with mixed
signs.

### Framing placement counter (bygg, 2026-10-30)

Each pipelined framing job now clones the writer's `Executor` (one `Arc`
increment and decrement) and, on success, checks whether it runs on a
worker of that executor (one thread-local read) and adds one to
`writer_payload_framed_on_executor_items`. The synchronous writer is
untouched, so its arm is the control. Same gate: `brokkr write`, denmark
`--variant indexed --bench 1 --compressions none`, cells interleaved.
Baseline `--commit e36fa38f` (stored); variant the counter tree on top of
it, measured uncommitted with `--force` (not stored, no UUID).

| Writer | Baseline pair 1 | Variant pair 1 | Baseline pair 2 | Variant pair 2 | Medians |
|---|---|---|---|---|---|
| sync | 6390 ms (`d8e3ff65`) | 6498 ms | 6403 ms (`828f53ee`) | 6486 ms | 6397 vs 6492 ms |
| pipelined | 5687 ms (`0f8f6011`) | 5749 ms | 5694 ms (`bdea0d40`) | 5808 ms | 5691 vs 5779 ms |

Below noise: the pipelined arm is 1.5 % slower in both pairs, and so is
the unchanged sync arm, so the shift is the build or the drive between
the worktree and the working tree, not the counter.

### Input eviction counter (bygg, 2026-10-30)

Every input eviction (`POSIX_FADV_DONTNEED` behind the read head, once per
blob under `PageCachePolicy::Evict`, which the CLI uses) now mints a nested
call on the invocation and adds one to `fadvise_input_evictions`, beside the
syscall it counts. Under `Keep` the branch is never armed. Gate: `brokkr
tags-filter -R`, denmark `--variant indexed --bench 1`, whose single-pass
path reads through the evicting `ElementReader` (the default two-pass path
reads by pread walkers and evicts at most its header reads, so it would not
exercise the counter). Baseline `--commit 8b91fae9` (stored); variant the
counter tree on top of it, measured uncommitted with `--force` (not stored,
no UUID). Cells interleaved.

| Baseline pair 1 | Variant pair 1 | Baseline pair 2 | Variant pair 2 | Medians |
|---|---|---|---|---|
| 200.8 ms (`15fbaa3b`) | 200.8 ms | 200.9 ms (`224a6659`) | 200.9 ms | 200.8 vs 200.8 ms |

No cost. The run is short, so this bounds the counter to well under the
2-3 % noise floor of a 200 ms wall, not to its absolute per-blob cost.

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

Denmark: 0 interpolation ways (Scandinavian precise addressing). Germany's
interpolation counts are under "Interpolation ways as OSM tags them" below.

### Records and lists decoded through the format codec (bygg, 2026-11-09, `d510bbd7` vs `067be2d8`)

`64141296` gave the index's records and list framing one owner in
`format.rs` (`record!`, `Table`, `ListCount`); `067be2d8` made the builder
read its own pass 2 outputs through it (`Table::get` and `span_at` in pass 3
stage A and interpolation resolve, `BucketRecord` in stage B). Index bytes
unchanged. Planet reindexed input (`e7f431ab`), both arms as `--commit`
worktrees (cancels the worktree build offset), untrimmed (`fstrim` was not
available to the session). One discarded cell per arm first (`01d9d6cf`
323.8 s, `9b2a3902` 310.4 s), so every measured cell follows a full geocode
build; then A B B A, `--bench 1`:

| Cell | Commit | Wall | Stage A streets | Stage B fine | Stage B coarse |
|---|---|---|---|---|---|
| `363d5fee` | `d510bbd7` | 311.8 s | 29.6 s | 21.2 s | 10.4 s |
| `85f54bbd` | `067be2d8` | 314.1 s | 30.4 s | 20.9 s | 9.6 s |
| `4dc8d348` | `067be2d8` | 313.8 s | 30.2 s | 20.7 s | 9.8 s |
| `e0fb682f` | `d510bbd7` | 313.2 s | 29.5 s | 21.7 s | 10.2 s |

Medians 312.5 s against 314.0 s (+0.5 %), both pairs the same sign
(+0.7 %, +0.2 %): no material regression for the two commits combined on
this workload. Stage A streets is consistently about 0.7 s (2-3 %) slower
on `067be2d8`, offset by stage B; every other phase, Pass 2A/2B, flush,
interpolation resolve and admin index included, is within noise. A screen of
the pair, not a per-commit bound and nothing about HEAD (both arms predate
`25172fc3`'s malloc change, hence about 313 s, not about 176 s). The build's
wall includes reader-open validation and one smoke query; reader query
throughput and latency across the pair remain unmeasured. The July 347 s
figure (plantasjen, old input) was never a baseline for this pair.

### addr:place addresses and numeric steps, format 5 (bygg, 2026-11-08, against `fb5f05cc`)

`AddrPoint` grows 20 to 24 bytes (`place_offset`), `InterpWay` 23 to 24
(`parent_kind`); address points also come from `addr:place` (ADR-0027).
Planet (`planet-20260223-reindexed`), interleaved `--bench 1` cells,
variant uncommitted (`--force`, dirty sidecars):

| Cell | Arm | Wall | Pass 2a |
|---|---|---|---|
| `82d91ad9` | baseline | 178.3 s | 21.4 s |
| `c0ffdb9a` | first variant | 188.8 s | 30.2 s |
| `d0549866` | baseline | 175.2 s | 21.3 s |
| `136bdef0` | first variant | 186.5 s | 30.2 s |
| `c9273b60` | final variant | 176.5 s | 21.7 s |
| `bcb822dd` | baseline | 172.6 s | 21.2 s |
| `a1efce29` | final variant | 176.3 s | 21.5 s |

Baseline median 175.2 s, final variant 176.4 s: no wall difference.
The first variant was 6 % slower, all in Pass 2a, whose serial merge became
the bottleneck (`ordered_pool_result_send_wait_ns` at its end 41 s to 91 s
summed over workers). Neither the node tag loop (restored to the old one:
still slow) nor the interning shape moved it. On Germany a 1 MiB write
buffer for the streamed data files (`GEOCODE_DATA_FILE_BUFFER_BYTES`)
restored Pass 2a (0.95 s baseline, 1.44 s variant, 0.95 s with it); at
planet it did not (29.7 s), and moving the address merge loop into one
out-of-line function shared by both merges did (21.7 s). A `--hotpath`
build of the slow variant also ran Pass 2a in 21.5 s, so the slowdown was
code generation of the inlined merge, not work. The remaining
difference is the segment after the admin index, which writes the index
files (about 14 to 16.5 s, 24.8 to 26.0 GB written): the larger records.
The variant planet index holds 167,816,443 address points; no baseline
count was taken. Only `82d91ad9` of the baseline cells is stored in
`results.db`.

### Interpolation ways as OSM tags them (plantasjen, 2026-11-05, against `f4838f76`)

Interpolation ways no longer need `addr:street` on the way, and interior
address nodes split a way into one record per section (ADR-0026).

Germany (`germany-20260224-seq4704`), variant uncommitted: 2,536
interpolation ways classified (78 before: only those with the street on
the way), 2,228 resolved (7 before), 308 unresolved, 2,724 records. Wall
19.9 s.

Planet (`planet-20260223-reindexed`), one matched `--bench 1` pair:
baseline `70e9c975` at `f4838f76` 299.4 s, variant (uncommitted, dirty
sidecar `954f1ba5`) 295.2 s - no wall difference. Interpolation phases:
`GEOCODE_INTERP_RESOLVE_INDEX` 0.86 to 2.03 s (the coordinate table now
covers every node of every way), endpoint and section resolution 0 to
0.11 s, `GEOCODE_PASS3_STAGEA_INTERP` 0.02 to 0.61 s: about 2 s more work,
below the run-to-run band. Unresolved ways 34,049 to 302,181 (the classified
set grew far more; the resolved count is not exported as a counter).

### Length-prefixed string pool (bygg, 2026-11-04, against `47f5a973`)

`strings.bin` entries carry a LEB128 length instead of a NUL terminator
(ADR-0025): interning writes the prefix and deduplicates on the decoded
length and bytes, the reader decodes the prefix instead of scanning for
the NUL, and open checks the pool's sentinel once. Planet, `--bench 1`,
two pairs: A stored `08eed2c8` (at `3c5b8d3a`, geocode code identical) then
B uncommitted 175.5 s (A 172.4 s); A `b1b8b1bb` (clean `47f5a973`) 174.1 s then B
uncommitted 173.7 s. A's mean 173.3 s, B's 174.6 s (+0.8 %), noise. The
first pair's +3.1 s sits in Pass 1, a relation scan at 0.5 cores that read
87 GB against the stored cell's 77 GB, not in a phase that interns or
reads strings; Pass 2b, where most interning runs, moved +0.8 s and +0.4 s
(3 % and 1 %). Peak RSS unchanged (13.3 GB in all four cells).

### Interpolation ends from their own nodes (bygg, 2026-11-03)

The endpoint resolver takes each end's number from the same-street
addresses at exactly the end node's coordinate (ADR-0024) instead of the
nearest one in the end's street cell and its eight neighbours. The lookup
holds only the address points at some end coordinate, found by one
parallel pass over the address table against a set of end coordinates, in
place of the S2 projection and sort of every address point. Planet,
`--bench 1`: A is clean `7e7ede73` (`a450ea22`); B is the change on
`76e9fd2f`, uncommitted (`--force`, not stored).
`GEOCODE_PASS2_INTERP_RESOLVE` 1511 ms to 457 ms (index build 1494 ms to
455 ms). Unresolved ways 24,534 to 34,049. Wall 179.0 s to 189.1 s, not
attributed to this change: the phases that grew (Pass 3 Stage B fine
21.5 s to 30.0 s, the tail after the admin index 15.7 s to 19.5 s) do not
depend on how a way resolved, since Pass 3 lists resolved and unresolved
ways alike, and the cover change between A and B (`edefd624`) moved Stage
B fine only 21.5 s to 22.3 s in its own pair. Germany: 71
of 78 resolved at street level 17 and 77 at level 16 before (level 16
rerun on `76e9fd2f`), 7 at both levels after.

### Vertex cells seeded in the segment cover (bygg, 2026-11-03)

`cover_segment` emits the cells holding both stored vertices directly
instead of relying on the t=0 and t=1 samples to reach them, and skips
those two samples when they are bit-identical to their vertex. Planet,
`--bench 1`: A is clean `603068a1` (`5e1fbae5`), 177.1 s; B is the change
on that commit, uncommitted (`--force`, not stored), 178.5 s, noise. Pass 3
Stage A streets, the only phase that runs the per-segment cover, went
20.5 s to 18.9 s: the skip saves two cell computations per segment, more
than the two seeded insertions cost. A first B cell of the seeding without
the skip ran 188.6 s, with Pass 2A and 2B (which never cover a segment)
reading 4 to 7 GB more from disk than A and Stage A streets at 22.3 s. The
stored 320 s planet cells from before the malloc arena change are no
baseline for this build any more.

### Conservative path cover replaces the segment sampler (bygg, 2026-11-03)

The segment cover walks each lat/lon path's great-circle chords over the
S2 grid of every face they reach instead of sampling at 0.4 of the smallest
cell width. Planet, `--bench 1`: A is clean `7e7ede73` (`a450ea22`),
179.2 s; B is the change on that commit, uncommitted (`--force`, not
stored), 175.4 s. Pass 3 Stage A streets 18.9 s to 17.6 s (one sphere point
per node instead of per segment end, no per-segment hash set), admin cells
3.6 s to 2.9 s, Stage B fine 21.5 s to 22.3 s (more cells to group). Denmark
at the same pair: fine cells 1,712,384 to 1,747,381 (+2.0 %, the cells the
sampler missed), coarse 119,012 to 119,109, admin 1,223 to 1,231.

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

### Query latency: the radius searched in full (bygg, 2026-11-03, on `edefd624`)

brokkr has no geocode query command, so these come from an uncommitted
probe example that opened one index and timed `Reader::query` over fixed
pseudo-random points, alternating rounds of the new search ("walk") and of
the old nine-cell search kept in the same build ("nine"), 3 to 6 rounds per
cell. Per-query means, warm page cache; the host's speed drifted between
cells, so compare within a row only:

| Index | Points | nine | walk | Delta |
|---|---|---:|---:|---:|
| Denmark | Copenhagen bbox, uniform | 65.6 us | 66.2 us | noise |
| Denmark | country bbox, a street within 75 m ("on road") | 22.5 us | 22.4 us | noise |
| Denmark | country bbox, uniform (over half open sea) | 18.5 us | 20.0 us | +8 % |
| Germany | Berlin bbox, uniform | 188 us | 194 us | +3 % |
| Germany | country bbox, on road | 112.7 us | 113.5 us | noise |
| Germany | country bbox, uniform | 90.6 us | 94.4 us | +4 % |

Admin point-in-polygon dominates every row (Germany's admin polygons carry
up to 16x the base vertex budget). The walk costs extra only where an answer
lies beyond the 3x3 block's proved reach or nothing lies near at all, which
is where the nine-cell search was wrong or incomplete: on the uniform
Germany points it answered 8.4 % of streets and 10.4 % of addresses with
nothing or a farther one (against indexes built at street level 16 and
coarse level 12, whose nine cells reach past the radii, and which the walk
matches exactly on 1.3 M class answers across both datasets). A first
version that walked every cell within the radius on every query cost +27 %
on the uniform Denmark points and +4 % on Germany on-road points (the coarse
search for interpolation, absent nearly everywhere, ran on every query); the
block bound and the presence test brought both rows down. The Berlin row
predates the presence test.

The build is unchanged: Denmark `--bench 3 --force`, 1770.6 ms against the
stored `0bf93c83` at `7e7ede73`, 1929.8 ms.

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

### Unindexed `.osc.gz` assembly (bygg, 2026-11-04, at `80e0b8f0`)

The one path that still assembles through `ParallelGzipWriter`: an
`.osc.gz` from inputs without indexdata, so the comparison is the
sequential element stream and the gzip pool runs on every host thread
(32 here) after it. `diff-snapshots --dataset germany --from base --to
20260420 --variant raw --format osc --bench 1`, `217680f2`, 104.3 s:

| Span | Duration | Peak anon | Avg cores |
|---|---:|---:|---:|
| node, way, relation scan | 103.8 s | 14 MB | 1.0 |
| assembly (`DERIVECHANGES_WRITE`) | 0.36 s | 71 MB | 12.5 |

About 1.8 GB of fragment XML in, 362 MB written. The pool never waited
for a sequence permit (`writer_parallel_gzip_reorder_blocked` 0,
reorder high water 9 slots), and its 12.5 cores of 32 put the limit on
the fragment copy feeding it, so this run used little of the writer's
envelope (about 292 MiB of payload capacity at 32 workers; the bound
and what it assumes are at `ParallelGzipWriter::new`). That envelope
is reached only under backpressure (slow output, a straggling early
chunk, a pool slower than its producer) and grows with host threads.
The assembly is nonetheless this command's peak: the scan's readers
and decode pool are dropped before it, so it does not stack on the
scan's working set. 0.36 s is three 100 ms samples, so 71 MB is a
sampled peak, not the instantaneous one.

### Unindexed text diff across its three reshapes (bygg, 2026-11-09)

Script bench `diff-unindexed` (`benches/diff/diff-unindexed.lua`):
`diff -c` of germany `pbf.raw` (base, seq 4704) against germany
snapshot `20260420` `pbf.raw`, both without indexdata, so every arm
takes the sequential element stream. Inputs evicted and fincore-checked
at zero before each run (the 9.4 GB pair does not stay cached under the
bench's memory limit, so warm cannot be held). One iteration per
invocation, both arms `--commit` worktree builds, A B B A per pair;
walls are the diff child's own. Every arm of a pair wrote the same
output bytes and the same summary: type all 150641304 bytes, 7952543
differences, 495644074 common; `--type way` 31301484 bytes, 1601349
differences.

| Pair | Type | Before (s) | After (s) | Change |
|---|---|---|---|---:|
| `8881415e^` vs `8881415e` | all | 152.9, 151.5 | 156.5, 161.9 | +5 % |
| `8881415e^` vs `8881415e` | way | 153.6, 162.0 | 102.6, 103.5 | -34 % |
| `94743e65^` vs `94743e65` | all | 151.3, 151.7 | 97.4, 100.0 | -35 % |
| `126cbce2^` vs `126cbce2` | all | 98.5, 100.3 | 96.6, 96.6 | -3 % |

Uuids in run order: `fb596e36` `96fe93aa` `0eb42526` `6a9853f6`;
`214b4ee4` `f050f528` `ea2a5a3a` `2ae91082`; `5f2e009e` `e9d40917`
`72230dd3` `d8eecbd8`; `e15b137d` `4d441144` `992273d3` `af661a0e`.

- `8881415e` (per-element order check, blocks classified by their
  elements, excluded kinds dropped instead of converted and merged):
  slightly slower on the full diff, as expected; under `--type way` it
  saves a third, the node and relation drains no longer converting
  elements. The saving lies outside the `DIFF_PHASE_WAY` span, so judge
  `--type` arms on the whole wall.
- `94743e65` (borrowed elements instead of owned conversion): a third
  off the full diff, and peak anon 102 MB -> 21 MB (`5f2e009e` vs
  `e9d40917`); the two sides' block peaks coinciding costs nothing
  visible.
- `126cbce2` (one `DecompressPool` per merge): within the noise floor,
  peak anon 21 MB -> 24 MB (sampled); `5bfa37ab` later changed the
  pool's retention, so this is the change as landed, not as it is now.

## Diff of a base against its apply-changes output (bygg, 2026-10-31)

`brokkr diff`: the base against the base with one daily OSC applied, run
as `pbfhogg diff OLD NEW --suppress-common --output FILE` (so the
byte-equal skip may fire), `--keep-cache`, `--bench 1` cells interleaved,
no `fstrim` (not allowlisted). All rows exit 1 (the inputs differ).

### Planet, default `-j` (one shard per core), OSC 4913

| UUID | Commit | Wall | DIFF_SCAN (header walk) | Node phase peak anon |
|---|---|---:|---:|---:|
| `789b28b4` | `79d2e923` | 401.8 s | 82.8 s | 937 MB |
| `07380793` | `2c12a955` | 397.2 s | 87.5 s | 692 MB |
| `7f085114` | `79d2e923` | 412.1 s | 91.0 s | 933 MB |
| `a43d9f58` | `2c12a955` | 403.6 s | 92.7 s | 690 MB |
| `e95703aa` | `f3e1c9b9` | 395.4 s | 95.1 s | 653 MB |

`2c12a955` gave the sharded default path the byte-equal skip it lacked.
On this workload it is worth -1.1 % and -2.1 % in the two pairs (medians
407.0 s against 400.4 s, below noise) and about a quarter of the peak
anon: the skip fires on 7,145 blob pairs (2.1 G elements), while 43,669
overlapping pairs (9.9 G elements) still decode, because one day of
edits touches most of planet's large blobs. The Japan figure that
motivated the skip (`diff -c` 50.0 s to 6.8 s, plantasjen, April) came
from the sequential engine on a Geofabrik-density file and does not carry
to planet. `diff -q` runs the same engine with the same skip and was not
measured separately (brokkr's `diff` never passes `-q`).

The serial header walk was a quarter of the wall: about 1.28 M `pread`s
and 10 GB read at 0.0 cores. Walking the two files on two threads was
tried (uncommitted, `f3e1c9b9` plus the change) and measured 98.8 s, no
gain, so it was not kept. The "new" input brokkr builds with
apply-changes is 174 GB, not 92 GB: the two inputs total 266 GB, so the
merge phases' 258 GB read was no over-read.

### Planet, streaming work units (bygg, 2026-10-31)

`4dbc2d47` (the engine landed in `587bfc12`) reads both inputs once as
sequential streams cut into work units on the ordered pool; no header
walk, and the text goes to the output in order. Matched `--bench 1`
cells, same OSC 4913 setup:

| UUID | Commit | Wall | Peak anon |
|---|---|---:|---:|
| `05d47098` | `4dbc2d47` | 284.1 s | 1.81 GB |
| `ec05b5d5` | `9afb9449` (header walk, shards) | 384.1 s | |
| `b7e58fb6` | `4dbc2d47` | 284.6 s | |

-26 %. The node phase runs at 29.6 cores with 248 GB read from the
266 GB of input, each byte once. Peak anon rose from about 0.65 GB: it
is the unit window, `workers + DIFF_UNIT_WINDOW_SLACK` units of
`DIFF_UNIT_TARGET_BYTES` frames plus the planner queue and decoded
blocks. An uncommitted 16 MiB target measured 295.4 s at 1.18 GB peak
anon, about 4 % wall for a third of the memory; 32 MiB was kept.

`diff-snapshots` (base against `20260411`, two independently cut
planets) shares the engine:

| UUID | Commit | Format | Wall |
|---|---|---|---:|
| `aaf851c9` | `4dbc2d47` | text | 116.5 s |
| `ca13a5b8` | `9afb9449` | text | 129.9 s |
| `9dc54efe` | `4dbc2d47` | osc | 210.5 s |
| `11143d70` | `9afb9449` | osc | 215.7 s |

Text -10 %; osc within noise.

### Derive units deflate and spill their own output (bygg, 2026-10-31)

`diff-snapshots --from base --to 20260411 --format osc -j 16`, matched
interleaved `--bench 1` cells, base `179f1b7e` (stored) against the
same tree plus worker-side gzip members and per-unit spill (unstored,
`--force`, landed in the commit following `179f1b7e`):

| Cell | Commit | Wall | Peak anon |
|---|---|---:|---:|
| `3660071f` | `179f1b7e` | 240.7 s | 5.00 GB |
| (unstored) | variant | 156.3 s | 1.25 GB |
| `955da7f4` | `179f1b7e` | 241.3 s | |
| (unstored) | variant | 156.6 s | |

-35 %, sign-consistent across both pairs. The base's phase split
(`c55bebc9`, same commit) had the merge at 15.6 cores for 101 s, then
37 s, 14 s and 15 s of node, way and relation tails at 1.1-1.4 user
cores and 0.1 kernel: not the calling thread's scratch writes but
head-of-line formatting. A unit the consumer was not reading could run
only `DIFF_OUTPUT_CHUNK_DEPTH` chunks ahead, and the 440 new-only node
blobs (109 M elements, about 34 MB of XML each) made each tail unit's
output hundreds of MB, so only the head unit's worker progressed. The
same blobs made each chunk a whole blob's XML, which was the 5.0 GB
spike as the tail began. In the variant those tails run at 12.2, 7.4
and 13.0 cores in 5.9 s, 4.1 s and 1.4 s, and the assembly is a 1.6 s
copy of compressed fragments instead of 8.2 s deflating 29 GB. The text
form, which shares the engine but neither change, is neutral: `c12221f5`
(`179f1b7e`) 146.3 s against 146.4 s unstored.

### Single-sided stretches cut by claimed elements (bygg, 2026-10-31)

`diff-snapshots --from base --to 20260411 -j 16`, matched interleaved
`--bench 1` cells in A B B A order, base `c7eed182` (stored, built by
`--commit`) against the same tree plus a unit cut once its single-sided
blobs claim `DIFF_UNIT_LONE_ELEMENTS` elements (unstored, `--force`,
landed in the commit following `c7eed182`):

| Form | Cell | Arm | Wall | Peak anon | Node tail | Way tail |
|---|---|---|---:|---:|---:|---:|
| text | `258cbdc4` | base | 145.6 s | 1.57 GB | 4.55 s | 2.13 s |
| text | (unstored) | variant | 141.8 s | 1.30 GB | 0.25 s | 0.25 s |
| text | (unstored) | variant | 142.3 s | 1.30 GB | | |
| text | `ed12c024` | base | 146.5 s | | | |
| osc | `6d806707` | base | 156.8 s | | | |
| osc | (unstored) | variant | 156.0 s | | | |
| osc | (unstored) | variant | 156.2 s | 1.26 GB | | |
| osc | `4b6f07ee` | base | 157.0 s | 1.25 GB | | |

Tails are the `--durations` overlap of a kind's END past the next
kind's START. The text base's anon sat at a 1.1 GB plateau (unit frames
and decoded blocks) through the node merge, then jumped by 0.43 GB at
the moment the tail began and drained linearly over 4.5 s at about
3 user cores: each 32 MiB tail unit held about 18 new-only planet blobs
(318k nodes each) of text, about 80 MB, against a channel of four
blob-sized chunks, so every unit behind the head parked full and the
head formatted alone. One-blob tail units fit their channel whole, so
the window formats them in parallel. Text -2.8 % wall and -0.27 GB
peak, sign-consistent across both pairs; osc, whose spill already
removed the shape, is neutral.

### Europe, `-j 1`, OSC 4715

| UUID | Commit | Wall | DIFF_SCAN | Peak anon |
|---|---|---:|---:|---:|
| `cae9f615` | `79d2e923` (streaming sequential engine) | 193.0 s | none | 14.6 MB |
| `ea3d4f41` | `2c12a955` (header walk, then one pread shard) | 270.4 s | 79.3 s | 106 MB |
| `9503a2ce` | `f3e1c9b9` | 255.3 s | 79.3 s | 96.9 MB |
| (unstored) | `f3e1c9b9` plus the `-j 1` stream, two cells | 177.1 s, 175.6 s | none | 18.0 MB |

`2c12a955` cost `-j 1` 40 %, all of it the up-front walk of both files:
the merge phases matched the streaming engine to within a second. One
shard now reads each input as one sequential stream, checking each
header as it arrives, which removes the walk and brings back
`--direct-io` and page-cache eviction at `-j 1`: -31 % against
`f3e1c9b9`. The unstored cells also carry `f3e1c9b9`'s cheaper block
construction, so they compare with `79d2e923` only approximately.

## Common lines cut by elements (bygg, 2026-11-09, against `71976fd7`)

`pbfhogg diff OLD NEW` without `-c` on Denmark indexed against its own
`cat --dedupe` output: equal elements in different framing, so no blob
pair is byte-equal and every one of 59,152,282 elements prints a common
line. The diff child of `dedupe-self --cache warm --check yes` cells, one
run each, input warm, 16 workers:

| Cell | Build | diff child | send wait | head wait |
|---|---|---|---|---|
| `89f2ce1e` | `79d2e923`, sharded engine, text through scratch (`--commit`) | 1.29 s | - | - |
| `f6d7708e` | `4e0eaa50`, work units (`--commit`) | 8.22 s | 128.8 s | 7.86 s |
| `2e89d66e` | `71976fd7` plus every frame counted (main tree, dirty) | 0.65 s | 0 | 0.43 s |

"send wait" is `ordered_pool_result_send_wait_ns`, "head wait"
`ordered_pool_result_recv_wait_ns`. At `4e0eaa50` a 32 MiB unit made about
30 MB of common lines against a channel of four 1 MiB chunks, so every
unit behind the head parked and the run formatted on one core. The
single-sided element cut (`DIFF_UNIT_LONE_ELEMENTS`) never fired, since
every frame overlaps the other input. Counting every taken frame's
claimed elements when common lines print cuts units of about 131k
elements, whose text fits the channel. The diff source is identical
between `4e0eaa50` and `71976fd7`; the variant arm is a main-tree build
against worktree builds (brokkr#105 measured worktree builds up to 18 %
slower in one CPU-bound phase), which does not touch a 12x difference.
No standard planet cell exercises the change: brokkr's `diff` and
`diff-snapshots` text cells pass `--suppress-common`, and `--format osc`
plans as before, so their units are unchanged.

The planet `diff-snapshots` A/B of `2c12a955^` against `2c12a955` was not
run: both arms are the sharded engine `587bfc12` replaced, and no code
decision rides on it. The stored `brokkr diff` pair at those two commits
(base against its apply-changes output, above) is a different workload
and is not a substitute for it.

## Text units hold pieces past a full channel (bygg, 2026-11-09, against `5432aac7`)

The text diff's sink now hands output over in exact `DIFF_OUTPUT_CHUNK_BYTES`
pieces and, once a unit's channel is full, holds further pieces in memory
against `DIFF_TEXT_OVERFLOW_BYTES` instead of blocking. `diff-text` script
bench (`benches/diff/diff-text.lua`), inputs warm, one iteration per cell,
diff child's wall; base arm a `--commit HEAD` worktree, variant the dirty main
tree:

| Input | Cell | Build | diff child | send wait | overflow chunks |
|---|---|---|---|---|---|
| denmark base vs `320k` repack, no `-c` | `66c7ae25` | `5432aac7` | 2.82 s | 0 | - |
| | `edea221b` | variant | 2.81 s | 0 | 0 |
| germany base vs `20260420`, `-c -v` | `dde2ffe5` | `5432aac7` | 5.51 s | 63 ms | - |
| | `bd12c886` | variant | 5.54 s | 3.6 ms | 0 |

No cost, and no gain: neither real input outruns a unit's channel, so the
allowance is never used. The denmark pair was meant as an oversized-frame
input and is not one: a 320k-element frame on one side is carried and
clipped at the other side's cuts, so units stay at the element cut's size
(493 node units); that run is limited by the planner and head (job lock wait
17 s, head recv wait 2.4 s summed), not by send waits. Two months of
germany changes under `-v` (3.46M modified, 440 MB of text) waited 63 ms
in sends at the base; the variant's exact pieces cut that to 3.6 ms. The
shape the mechanism exists for (many modified elements with long details, or
both inputs framed past any cut) is in no dataset, so its gain is unmeasured.

## Inspect --extended: validator-projection scan (bygg, 2026-10-31)

`4dbc2d47` scans each blob inside the block validator instead of
building a `PrimitiveBlock` and walking its elements. Planet,
interleaved `--bench 1`:

| UUID | Commit | Wall | Peak anon |
|---|---|---:|---:|
| `970ff659` | `4dbc2d47` | 27.1 s | |
| `3970b8a1` | `587bfc12` | 39.5 s | |
| `c1a2a0e6` | `4dbc2d47` | 26.4 s | 303 MB |
| `102cf3b2` | `587bfc12` | 39.7 s | 379 MB |

-33 % wall. Every decoding inspect mode runs the same scan.

## check --refs and getid read through projections (bygg, 2026-11-01)

check `--refs` and `getid --add-referenced` read each blob through a
projection on the block validator (`src/read/project_ids.rs`, and getid's
include probe) instead of building a `PrimitiveBlock` and walking its
elements, as check `--ids` and getparents already did.

What the element walk cost (base, `25172fc3`, planet): check `--refs`
hotpath `ecfaf4c7` spent 291 core-s in `decompress_blob_raw` out of about
830 core-s in its two phases (nodes 17.7 s at 26.1 cores, ways 13.3 s at
27.5); the rest was validation, the offset tables, the element walk and
the set lookups. `getid --add-referenced` (`4c08d663`) ran pass 2 for
32.8 s at 29.0 cores over every node and way blob, walking every element
with its metadata to test one ID against a set; pass 1's pool ran 10.5 s
at 26.4 cores walking every way blob.

Planet `planet-20260223-with-indexdata`, interleaved `--bench 1`, base
`25172fc3` (stored, `--commit`) against the working tree (`--force`, not
stored). Spans are `--durations` START/END pairs.

| Command | Base wall | Projection wall | Main spans, base to projection | Peak anon |
|---|---:|---:|---|---|
| check `--refs` | 38.4, 38.5 s (`752a3925`, `913fd7fe`) | 31.6, 31.8 s | nodes 18.3-18.5 to 13.5-13.6 s, ways 13.2-13.3 to 11.6 s | 2.24 to 2.17 GB |
| `getid --add-referenced` | 49.9, 50.4 s (`232979ea`, `0b6748dd`) | 40.9, 41.0 s | pass 2 32.8-33.0 to 25.0-25.1 s, pass 1 17.1-17.4 to 15.8 s (6.5-6.8 s of it the schedule walk) | 467 to 460 MB |
| getid (include) | 113.7, 114.5 ms (`29f5741e`, `515b464c`) | 119.4, 119.7 ms | `GETID_SCAN` 99-100 to 101 ms; the rest is `PUBLISH` | |

check `--refs` -18 % wall, `getid --add-referenced` -18 %. Both nodes
phases are now read-bound: check `--refs` nodes ran at 20.6 cores
against 25.2 (58 GB in 13.6 s), getid pass 2 at 18.2 cores against 29.1
(67 GB in 25 s). The include walk decodes only the handful of blobs whose
indexdata range holds a requested ID, so its probe changes nothing
measurable; its few-millisecond gap sits in output publication, outside
the changed code. The first include base cell (`31c13dc7`, 6.9 s) read
cold headers after the add-referenced cells and is not a pair.
`getid --invert` was not measured: it builds every block it decodes as
before, the only change being that construction and the kind-claim
refusal are one call instead of two.

## getid --add-referenced follows relation members (bygg, 2026-11-03, against `7bdd2813`)

`getid --add-referenced` gained osmium's relation closure: one split
header walk serves a relation read (`GETID_RELATIONS`: the requested
relations' members and every relation-in-relation edge), a second
relation read only when the closure reaches a relation not requested, and
the way read (`GETID_WAYS`) over the requested and pulled-in ways. The
bench ID set requests three relations, so the variant cell does more work
than the base: its relations pull in 9 more ways and about 400 more nodes
(`getid_dep_way_ids` 12, `getid_dep_node_ids` 472 against 76), and no
relation in a relation is reached, so the second read does not run.

Planet `planet-20260223-with-indexdata`, `--bench 1`, base `7bdd2813`
(stored, `4f9572e9`) against the working tree (`--force`, not stored):

| Cell | Wall | `GETID_RELATIONS` | `GETID_WAYS` (base: pass 1 after the walk) | Pass 2 | Peak anon |
|---|---:|---:|---:|---:|---:|
| base | 40.9 s | | 8.8 s | 25.2 s | 402 MB |
| edges in a `Vec`, dropped after the closure | 41.1, 41.2 s | 0.29, 0.30 s | 8.7, 8.6 s | 25.2, 25.4 s | 572, 591 MB |
| edges in their own anonymous mapping | 41.2 s | 0.31 s | 8.7 s | 25.3 s | 412 MB |
| control: no relation requested | 40.9 s | | 8.8 s | 25.3 s | 391 MB |

The relation read finds about 1.04 million relation-in-relation edges
(`getid_relation_edges`, 16 bytes each). Holding them in a `Vec` and
freeing it after the closure cost 170-190 MB of peak memory, none of it
live: `mallinfo2` after the way read showed a 541 MB arena with 0.2 MB in
use, against 35 MB in the control. Freeing a large mmapped block raises
glibc's dynamic mmap threshold, so the way read's and pass 2's decompress
buffers came from the arenas instead of mmap and stayed resident once
freed; leaking the `Vec` instead brought the arena back to 128 MB and the
peak to 445 MB. The store now lives in its own anonymous mapping
(`EdgeStore`), grown by remapping and unmapped after the closure, so it
never passes through `free`: wall and peak are within noise of the base.

## glibc's sliding mmap threshold across commands (bygg, 2026-11-04, at `3c5b8d3a`)

Question: does glibc's dynamic mmap threshold leave freed memory resident in
later work, which command pays it and how much, and does a fixed threshold
set at process start buy the memory back without wall cost. Answer: every
multi-phase command surveyed pays it, by 4 to 41 % of peak anon, and none of
the three fixed thresholds measured is a uniform win, so the CLI still sets
no allocator policy. Do not re-run this survey; the cells are below.

Mechanism (`research/glibc/malloc/malloc.c`, `__libc_free` and `sysmalloc`):
freeing an mmapped chunk of at most 32 MiB whose size exceeds the current
threshold raises the threshold to that chunk size and the trim threshold to
twice it. Some such free happens early in every command (a pool's decode
buffers at a phase end, any blob-scale buffer), and from then on blob-scale
buffers below the raised threshold come from the per-thread arenas, whose
freed interior memory stays resident (only `malloc_trim` releases interior
pages). `mallinfo2` shows it directly: check `--refs` after its way phase
holds a 148.5 MB arena with 0.2 MB in use (4.8 MB with the threshold
fixed); extract `--complete` ends with a 5.16 GB arena, 1.36 GB of it free,
its 4 MiB `IdSet` chunks arena-backed (`hblks` 3), against a 149 MB arena
and 1,025 mapped chunks with the threshold fixed. Fixing the threshold also
freezes the trim threshold, so a fixed-threshold cell mixes both effects.

Method: the arms differ only in `GLIBC_TUNABLES=glibc.malloc.mmap_threshold=N`
(glibc applies it through `do_set_mmap_threshold`, which disables the
sliding threshold exactly as `process::set_mmap_threshold` would; passed to
the child through `scripts/envrun.sh`). The default arm is the binary as
shipped. One binary, `--bench 1`, 32 threads, glibc 2.43, THP `always`,
`planet-20260223-with-indexdata`; brokkr records no `GLIBC_TUNABLES`, so
every fixed-threshold cell is an unstored `--force` cell, and so are the
later default cells interleaved with them. Peak anon is the sidecar's
`anon` max; the peak phase is the sidecar segment that holds it, named by
the marker that begins it. extract cells use `--bbox=-25,34,45,72`,
apply-changes OSC 4913 with `--compression zstd:1`.

### The survey: default against a threshold fixed at 128 KiB

128 KiB is glibc's starting value, so this arm is the process as if no free
had ever raised the threshold.

| command | arm | UUID | wall | peak anon | peak phase |
|---|---|---|---:|---:|---|
| `check --refs` | default | `7592610c` | 31.5 s | 2,183 MB | ways |
| `check --refs` | 128 KiB | `5a19bf9d` (unstored) | 31.4 s | 1,983 MB | ways |
| `tags-filter w/highway=primary` | default | `707198dd` | 75.6 s | 2,554 MB | pass 2 |
| `tags-filter w/highway=primary` | 128 KiB | `03b5f0b7` (unstored) | 76.0 s | 2,276 MB | pass 2 |
| `tags-filter r/route=bus` (runs the relation closure) | default | `4e5ca3bf` | 60.8 s | 2,592 MB | pass 2 |
| `tags-filter r/route=bus` | 128 KiB | `cdb0f07f` (unstored) | 54.1 s | 2,285 MB | pass 2 |
| `extract --simple` | default | `b0d3cb39` | 110.2 s | 3,127 MB | relation write |
| `extract --simple` | 128 KiB | `843a874f` (unstored) | 112.0 s | 2,549 MB | way write |
| `extract --complete` | default | `3ad0761a` | 119.8 s | 4,896 MB | `PREAD_WRITE_EXECUTE` |
| `extract --complete` | 128 KiB | `f3ed59b3` (unstored) | 124.2 s | 4,274 MB | `PREAD_WRITE_EXECUTE` |
| `extract --smart` | default | `12ebfc9c` | 133.6 s | 7,779 MB | pass 1 relation classify |
| `extract --smart` | 128 KiB | `53375530` (unstored) | 135.3 s | 7,442 MB | pass 1 relation classify |
| `repack` | default | `723e4963` | 148.3 s | 783 MB | ways |
| `repack` | 128 KiB | `b0024e40` (unstored) | 151.2 s | 559 MB | nodes |
| `degrade --strip-locations` | default | `7f7d260e` | 154.3 s | 778 MB | ways |
| `degrade --strip-locations` | 128 KiB | `70c3224b` (unstored) | 154.4 s | 552 MB | nodes |
| `cat --clean version` | default | `55c10efb` | 146.7 s | 776 MB | relations |
| `cat --clean version` | 128 KiB | `ff567413` (unstored) | 150.7 s | 690 MB | relations |
| `time-filter` | default | `bb2517a3` | 125.5 s | 668 MB | main scan |
| `time-filter` | 128 KiB | `bb5d2dbb` (unstored) | 126.7 s | 611 MB | main scan |
| `apply-changes` | default | `ed178984` | 96.4 s | 1,999 MB | after the way-to-relation transition |
| `apply-changes` | 128 KiB | `dba29045` (unstored) | 99.6 s | 1,471 MB | way phase |
| `check --ids --full` | default | `3dd9be05` | 29.2 s | 2,292 MB | ways |
| `check --ids --full` | 128 KiB | `6e479d57` (unstored) | 29.8 s | 2,126 MB | ways |
| `renumber` | default | `cd477c21` | 131.3 s | 3,271 MB | stage 2d |
| `renumber` | 128 KiB | `d62ada6b` (unstored) | 135.4 s | 2,307 MB | stage 2d |
| `getid --add-referenced` | default | `b0db023a` | 39.0 s | 461 MB | pass 2 |
| `getid --add-referenced` | 128 KiB | `62a4c1df` (unstored) | 41.2 s | 272 MB | ways read |
| `build-geocode-index` | default | `08eed2c8` | 172.4 s | 13,309 MB | pass 2b |
| `build-geocode-index` | 128 KiB | `76c97efb` (unstored) | 176.9 s | 12,874 MB | pass 2b |

The r/route=bus pinned wall is not the change: its pass-2 schedule walk
ran from page cache (57 ms against 6.5 s); its work phases are equal.

Per command, what the sliding threshold costs (the 128 KiB arm's saving):

- Affected, in the phase after the first pool ends: check `--refs` -9 %
  (way phase), check `--ids --full` -7 %, repack -29 % and degrade -29 % (the
  way phase, their peak, falls below the node phase), tags-filter two-pass
  -11 % and with the relation closure -12 % (way-dependency and pass 2),
  extract `--simple` -18 %, `--complete` -13 %, `--smart` -4 % at the run
  peak and -21 % in its write phase, renumber -29 % (stage 2d), getid
  `--add-referenced` -41 % (after the edge store already left the arenas),
  apply-changes -26 %.
- Affected inside one phase: time-filter -9 % and cat `--clean` -11 %. The
  effect is not only between phases: any early free raises the threshold
  for the rest of the run.
- Barely affected: build-geocode-index -3 %; its peak is the builder's own
  stores, not blob-scale buffers.
- Not affected by construction, not measured: commands that hold no
  blob-scale buffers across a pool (plain `inspect`, `cat` passthrough, `cat
  --type`, `sort` on sorted input, `merge-changes`, plain `getid`), whose
  planet peaks are small next to every row above.

What it costs in wall: 1 to 5 % on the commands that allocate blob-scale
buffers per blob, through mapping, faulting and unmapping each one. extract
`--complete` way classify 16.7 to 19.4 s (minor faults 0.68 M to 10.6 M)
and write 76.4 to 79.3 s (0.19 M to 14.4 M), apply-changes streaming 61.7
to 64.7 s, getid pass 2 25.1 to 26.4 s, cat `--clean` +2.7 %, renumber
+3.1 %. check `--refs`, tags-filter, degrade and time-filter are within
noise.

### Higher fixed thresholds: 1 MiB and 4 MiB

Interleaved cells, all unstored, in run order (default, then the fixed
arm, or the reverse after the `fstrim`).

| command | arm | UUIDs | walls | peak anon |
|---|---|---|---|---|
| `getid --add-referenced` | default | `d2a31f1e`, `0bb8cec6`, `5d2fcb80`, `cd3167f1`, `0c110f7f` | 39.8, 41.0, 41.0, 41.0, 41.0 s | 411, 441, 442, 425, 417 MB |
| `getid --add-referenced` | 1 MiB | `b9f84be1`, `9d9ff652`, `7b0a2d89`, `eb501852` | 40.8, 41.8, 41.6, 41.9 s | 263, 278, 279, 261 MB |
| `getid --add-referenced` | 4 MiB | `25fdc5e1`, `0285b427`, `bc09f777` | 40.3, 40.6, 41.1 s | 373, 368, 373 MB |
| `extract --complete` | default | `bd8e67ab`, `0857e34d`, `e4333b70` | 120.4, 119.8, 119.7 s | 4,880, 4,894, 4,878 MB |
| `extract --complete` | 1 MiB | `477613f2`, `c55f7329`, `71e15b3f` | 121.4, 120.6, 120.9 s | 4,549, 4,509, 4,537 MB |
| `extract --complete` | 4 MiB | `a0a0f0d2`, `dc314727`, `3d8e733a` | 121.2, 121.3, 121.0 s | 4,660, 4,608, 4,611 MB |
| `apply-changes` | default | `134f91d8`, `3ff91a4f`, then after `fstrim` `e44bdbee`, `8e1629d1`, `a7211d20` | 95.4, 99.6, 102.7, 97.0, 100.5 s | 1,899, 1,938, 1,901, 1,869, 1,903 MB |
| `apply-changes` | 1 MiB | `ff41305a` | 95.7 s | 1,579 MB |
| `apply-changes` | 4 MiB | `2b90f120`, `974babea`, `07c0709d`, then after `fstrim` `a4071c51`, `594adc58`, `298c3b62` | 92.3, 104.7, 102.5, 92.5, 97.8, 99.1 s | 1,777, 1,834, 1,686, 1,737, 1,872, 1,689 MB |
| `renumber` | default | `5aa13b41`, `48a247a7` | 136.3, 136.3 s | 3,308, 3,250 MB |
| `renumber` | 1 MiB | `10aca9cd` | 137.4 s | 2,506 MB |
| `renumber` | 4 MiB | `0b726f57`, `f1ee10e1`, `5b4d01cc` | 127.6, 136.4, 138.6 s | 2,364, 2,322, 2,294 MB |
| `cat --clean version` | default | `54121254`, `bf32566e` | 150.4, 150.3 s | 770, 779 MB |
| `cat --clean version` | 4 MiB | `493a355c`, `1bb2d473` | 149.3, 149.6 s | 865, 864 MB |

- 1 MiB keeps most of the saving, but getid pass 2 runs about 4 % slower
  (26.3 to 28.2 s against 25.1 to 25.4 s; 21 M minor faults against 0.2 M).
  The sequential blob reader allocates each compressed frame, about 1.8 MB
  at planet, fresh per blob (`BlobReader::next_frame`, `src/read/blob.rs`),
  so a threshold under the frame size maps and unmaps every frame.
- 4 MiB is wall-neutral where measured (apply-changes medians 98.5 against
  98.3 s over six cells each, getid, renumber, extract `--complete` +1.2 %)
  and keeps a smaller saving (getid -15 %, apply-changes -9 %, renumber
  -30 %, extract `--complete` -5 %), but raises cat `--clean`'s peak by
  about 90 MB (12 %). A threshold fixed at 4 MiB from process start routes
  every allocation below it through the arenas, where the default would
  have mapped those above the value it slid to; that is a plausible reading
  of the cat regression, not an attributed one.
- No fixed value predicts a win everywhere: the request sizes of the
  churned buffers (frames, encode and frame output) and of the
  residue-forming ones overlap, and an explicit trim threshold governs only
  the arena top, not the interior holes.

Gaps, stated so they are not mistaken for absences: the 1 MiB and 4 MiB
arms ran only on the commands above (not check `--refs`, tags-filter,
extract `--simple`/`--smart`, repack, degrade, time-filter, check `--ids
--full`, the geocode builder); `multi-extract`, `inspect --tags`, `diff`,
`cat --dedupe`, add-locations-to-ways and check `--locations` were not
measured at any fixed value. Where the extract `--complete` way-classify
and write phases allocate what they free per blob is not attributed.

## check --ids and getparents read through projections (bygg, 2026-11-01)

Both commands validate each blob through a `GroupProjection` on the block
validator (`BlockBuffer::project`) instead of building a `PrimitiveBlock`
and walking its elements. Block construction runs the same call with an
empty projection, so the refusals are the same code.

What the element walk cost (hotpath, HEAD `96243688`, planet, UUIDs
`037ca3b3` and `ea1f782f`): on check `--ids` the workers spent about
297 core-s in `decompress_blob_raw` and 157 core-s in construction
(validation and string-table UTF-8; the offset tables were 1.9 s of it)
out of about 737 core-s in the scan phases. Most of the rest was the
element walk, which for dense nodes stepped `id`, `lat`, `lon` and
`keys_vals` per node and for ways re-parsed every message the validator
had already parsed, all to read the ID. getparents spent 110 core-s
decompressing and 124 constructing out of about 350, the rest
re-parsing every way and relation to test refs and members.

Planet `planet-20260223-with-indexdata`, interleaved `--bench 1`, base
`96243688` (stored) against the working tree (`--force`, not stored).
Spans are `--durations` START/END pairs.

| Command | Base wall | Projection wall | Main spans, base to projection | Peak anon |
|---|---:|---:|---|---|
| check `--ids` | 34.9, 35.1, 35.1, 35.0 s (`c55ab749`, `f9b9bed4`, `bb0b6535`, `b0532d7b`) | 29.0, 29.0 s | nodes 18.0-18.3 to 13.7 s, ways 10.5-10.7 to 9.2 s | 556 to 542 MB |
| check `--ids --full` | 36.6, 36.7, 36.8, 36.9 s (`8045d024`, `10e7598f`, `932dacbf`, `4c6515ae`) | 29.8, 29.7 s | nodes 19.7-19.9 to 14.2-14.3 s, ways 10.8-10.9 to 9.4 s | 2.35 to 2.36 GB |
| getparents, walker arm | 18.96, 18.93, 18.94 s (`db9ddfb3`, `342927f3`, `3abd7aa8`) | 16.64, 16.70, 16.71 s | `GETPARENTS_DECODE` 13.2 to 11.0 s | 288 to 277 MB |
| getparents, FullScan arm (europe) | 11.78, 11.77 s (`a694cb57`, `35bba63f`) | 10.92, 10.89 s | `GETPARENTS_DECODE` 11.7 to 10.8 s | |

check `--ids` -17 % wall (`--full` -19 %), getparents -12 % on planet
and -7 % on europe's FullScan arm. The 5.7-6.1 s schedule walk ahead of
both is untouched. The check `--ids` nodes phase is now read-bound
(about 20 cores against 25 before, 58 GB in 13.7 s).

A first version copied each blob's IDs into a per-worker vector. Planet
node blobs hold a few hundred thousand nodes, so that vector kept a few
MB per worker for the whole phase: the nodes phase peaked at 637 MB
against 556 MB. Recording where each dense `id` array lies, and decoding
it once after acceptance, removed it at the same wall.

Construction itself moved onto the same path (validate, then append the
offset tables, rather than the reverse). check `--refs`, which still
builds blocks, kept its work phases at the same length: nodes 18.0 and
18.6 s base against 18.5 s, ways 13.5 s on both (`daa8ce1b`,
`7cb64fb5`).

## check reads an unindexed file once (bygg, 2026-11-01)

The header walk no longer puts an unindexed blob in every per-kind
schedule (`KindSchedules::Unindexed`). check `--ids` reads such a file in
one phase, judging each blob's IDs of every kind from one validation pass.
check `--refs` reads it in one combined phase that inserts each blob's
node IDs and judges its ways (and, under `--check-relations`, relations)
against the sets as they stand, reading again in the way or relation phase
only a blob whose judgement missed a reference.

Planet, interleaved `--bench 1`, base `2b1c6ba1` (stored, `--commit`)
against the working tree on it (`--force`, not stored). `pbf.raw` is
`planet-20260223.osm.pbf`, the indexed variant
`planet-20260223-with-indexdata.osm.pbf`.

| Command | Input | Base wall | Working tree wall |
|---|---|---:|---:|
| check `--refs` | unindexed | 51.25, 50.18 s (`1f1ac148`, `456dd944`) | 28.89, 28.84, 28.87 s |
| check `--ids` | unindexed | 69.58 s (`017965d8`) | 26.78 s |
| check `--ids --full` | unindexed | 70.32 s (`25c3d60b`) | 27.11 s |
| check `--refs` | indexed | 31.75, 31.53 s (`917d217e`, `d2648b33`) | 31.70, 30.64 s |
| check `--ids` | indexed | 29.53 s (`02167562`) | 29.68 s |
| check `--ids --full` | indexed | 30.42 s (`d81221ae`) | 30.26 s |

Unindexed check `--refs` -44 %, check `--ids` -61 %; the indexed cells
are unchanged within noise. The base's unindexed check `--refs` read the
file twice (`CHECKREFS_NODES` 22.5 s then `CHECKREFS_WAYS` 24.1 s, 86.8 GB
each at 22.5 cores); the working tree's `CHECKREFS_UNINDEXED` took 25.4 s
and the way phase re-read no blob (`checkrefs_way_blobs` 0 of 50,816):
on this sorted file no way's reference raced the node blob holding its
target. That one pass costs what the indexed file's two phases cost
together (13.5 + 11.6 s); the unindexed file's lower wall is its schedule
walk, 3.4 s against 5.7-6.1 s with indexdata to parse. Peak anon 2.13 GB
base, 2.00 GB working tree.

`--check-relations`, measured once brokkr's `check-refs` cell could pass
it: unindexed planet, interleaved `--bench 1`, base `2b1c6ba1` against
`ece03638` (both stored; `ece03638` carries the single-pass path above):

| Command | Input | `2b1c6ba1` wall | `ece03638` wall |
|---|---|---:|---:|
| check `--refs --check-relations` | unindexed | 72.62, 71.51 s (`09fe2081`, `75913388`) | 28.85, 29.15 s (`22a6c583`, `44119112`) |

-60 %. The base read the file three times (`CHECKREFS_NODES` 22.4 s,
`CHECKREFS_WAYS` 24.0 s, `CHECKREFS_RELATIONS` 21.5 s, about 86 GB each at
21.5-22.6 cores); `ece03638` read it once (`CHECKREFS_UNINDEXED` 25.3 s,
87.8 GB, 23.5 cores), and the relation phase re-read 2 of 50,816 blobs in
16 ms (`checkrefs_relation_blobs` 2, `checkrefs_way_blobs` 0): relations
then cost nothing over the plain check, 28.9 s. Peak anon 2.33 GB base,
2.24 GB `ece03638`.

## check --ids judges type order on unindexed input (bygg, 2026-11-01)

The unindexed pass of check `--ids` takes each blob's per-group kinds from
the validator (`GroupProjection::group_end` now carries the group's
`KindSet`, accumulated group-locally in place of the block-wide set it
joins) and reduces them to each kind's first and last `(blob, group)`
position. That touches every decode through the validator, by one set
union and one call per group. Planet, interleaved `--bench 1`, base
`ece03638` (stored, `--commit`) against the working tree on it
(`--force`, not stored):

| Command | Input | Base wall | Working tree wall |
|---|---|---:|---:|
| check `--ids` | unindexed | 26.86, 26.23 s (`e51fade9`, `7125af76`) | 26.42, 26.48 s |
| check `--ids` | indexed | 30.03, 29.71 s (`39471fc0`, `56b3604b`) | 29.58, 29.61 s |

Both within noise: no cost.

## check --ids --full proves an ordered kind duplicate-free (bygg, 2026-11-08, against `cbdb79f0`)

`--full` now runs the plain order scan first (recording negative IDs as
`IdOutOfRange`, as before); a kind in strict `osm_id_cmp` order cannot hold
a duplicate and is final, and only a kind found out of order is read again
with its pre-allocated `IdSet`. Planet `planet-20260223-reindexed.osm.pbf`
(indexed, ordered), interleaved `--bench 1`, base `cbdb79f0` (stored,
`--commit`) against the working tree on it (`--force`, not stored):

| Arm | Walls | Peak anon (ways phase) |
|---|---:|---:|
| base | 26.87, 26.65 s (`bf61b8f6`, `d32e145d`) | 2,283 MB |
| working tree | 25.98, 25.95 s | 446 MB |

The wall difference (about 3 %, same sign in both pairs) sits at the noise
line; the nodes phase loses the set's 1.9 GB of page faults (minflt 393k to
28k). The memory drop is the result. Not measured: an unsorted file, where
each out-of-order kind costs one extra read at the old peak.

## check --index and cat --reindex on a frame pool (bygg, 2026-11-08, against `74be4015`)

Both were one thread: read a frame, decompress, scan (`check --index`
scanned twice, IDs then tags). Now the frame walk stays sequential on the
calling thread (a stream is still an input) and feeds an ordered worker pool
(`read::frame_pool`, `workers + 2` frames in flight) that decompresses and
scans in one walk; results are folded in file order. Planet
`planet-20260223-reindexed.osm.pbf`:

| Command | Base `74be4015` | Working tree (`--force` / `brokkr run`, not stored) |
|---|---:|---:|
| `check-index --bench 1` | 494.2 s (`3965701e`), 1.0 cores, peak anon 15 MB | 28.9 s, 15.5 cores, peak anon 334 MB, read 3.1 GB/s |
| `cat --reindex` of `planet-20260223-with-indexdata` | 444 s (`brokkr history` 62313, at `3f8ed0ec`, dirty) | about 44 s (`brokkr history` 62538, 1.8 m minus a 64 s build) |

The base was CPU-bound on one core; the pool is near the drive's read rate.
The reindex output is byte-identical (`cmp`) to the sequential run's. One
cell per arm: the gap is 10 to 17 times, far outside any drift band.

## digest v2 cost (bygg, 2026-11-08, against `110f0520`)

What `digest --form full` (v2, the default) costs over `--form content`
(v1), and why. No earlier measurement claimed v2 was free: when v2 landed
(`16154b0d`) its success-path cost was recorded as unmeasured. The first
cells (2026-10-30) already held a same-commit form pair, content 500.8 ms
(`a18ba1f1`) against full 701.0 ms (`ee543355`) on denmark at `4157567d`,
with content equal to v1 at `d2d8836a` (500.7 ms, `b0da638b`): the
difference was the form, never drift. Those rows step in 100 ms; the
cells below do not.

Base `110f0520`, stored, interleaved `--bench 1`:

| Dataset | content | full | full over content |
|---|---:|---:|---:|
| denmark | 438.9 / 425.3 / 436.5 ms (`f06a04b1`, `5a4e73b1`, `32165455`) | 568.7 / 565.3 / 565.9 ms (`3d580b85`, `791dda31`, `51531a64`) | +30 % |
| germany | 3447 / 3444 ms (`780005c0`, `fd27696f`) | 5248 / 5176 ms (`7b930651`, `bed503ef`) | +51 % |
| planet (`planet-20260223-reindexed`) | 59.8 s (`9e7877b3`), 25.0 cores | 106.8 s (`d8fffd30`), 14.9 cores | +79 % |

Planet core-seconds were 1494 against 1592, only 7 % more work for 79 %
more wall. A planet blob holds about 200k elements, so one block's
full-form records run to about 14 MB, and every decode worker grew such a
buffer from empty per block and freed it on the consumer. A hotpath run
put the per-block serialization at 39 ms average, 1987 s of wall in it
against about 1470 core-s of CPU in the whole run: the workers were
off-CPU about a quarter of their time.

Two working-tree variants (`--force`, not stored; planet full, same host):

| Variant | planet full | note |
|---|---:|---|
| exact parallel XXH3: a 4-worker pool computes each 1024-byte XXH3 block's lane sums, the consumer folds them in order; digests bit-identical to base on denmark (both forms) and germany | 110.3 s, 15.8 cores | the hash pool sat idle; the decode side was the gate |
| the same plus recycled record buffers | 66.2 s | |
| recycled record buffers only, serial hash as base | 61.7 s; again 62.7 s | consumer about 89 % outside its receive wait, dispatcher admission wait 99 % |

So the cost was the per-block allocate-grow-free of blob-scale record
buffers, not the serial hash, and the parallel hash did not pay once that
was gone. What landed is the recycling alone: the ordered consumer hands
each hashed buffer back, cleared, for a later block (at most
`DEFAULT_DECODE_AHEAD` kept). The parallel hash was not kept.

Recycled buffers against base, working tree (`--force`, not stored), digests
identical to base on denmark content and germany full:

| Dataset | content | full |
|---|---:|---:|
| denmark | 380.2 ms (base ~436 ms) | 460.1 ms (base ~566 ms) |
| germany | 3340 ms (base ~3446 ms) | 3881 ms (base ~5212 ms) |
| planet | 53.1 s (base 59.8 s) | 62.7 s, 25.1 cores, peak RSS 1.42 GB (base 106.8 s, 14.9 cores, 1.62 GB) |

Full over content after the change: denmark +21 %, germany +16 %, planet
+18 %, about the extra bytes the full form serializes and hashes.

## Cost of appending the offset tables to the payload (bygg, 2026-11-01)

What block construction pays to append the string-table and group
offset tables to the decompressed payload (`WireBlock::inline_tables`),
the question being whether keeping the tables in separate per-block
storage would pay. Planet `planet-20260223-with-indexdata`, `--hotpath`
cells on `d7d4f967` plus a `hotpath::measure` attribute on
`inline_tables` (unstored `--force` cells):

| Command, buffer source | Calls | Avg | P50 | P95 | P99 | Total | Decode it rides on |
|---|---:|---:|---:|---:|---:|---:|---|
| cat `--clean`, pooled | 50,816 | 10.7 us | 7.7 us | 26.2 us | 44.5 us | 0.55 core-s | `decompress_blob_raw` 339 core-s, wall 154.6 s |
| diff-snapshots base to `20260411`, fresh buffer per block | 107,701 | 35.2 us | 9.6 us | 167.8 us | 272.4 us | 3.79 core-s | block-pair `decode` 1,113 core-s, wall 111.2 s |

The unpooled path's tail is consistent with the payload moving on the
growth (a buffer arrives exactly full), though the timer covers the
reservation and the serialization together. Even there the append is
0.3 % of decode work; spread over the run's cores it is about 0.1 s of
wall, a throughput share rather than a bound. Separate storage would not
remove the table bytes, only move them; what it would remove is a moving
realloc's transient old payload per active decoder and any allocator
retention of it, neither measured. It was not built. time-filter's
snapshot path builds a block per blob on the ordered pool, so it
appends like cat `--clean`; it was not measured separately.

## What the read pipeline's decode scratch holds (bygg, 2026-11-03, on `26db96fe`)

The pipelined reader's decode workers each keep a string-table and a
group-range scratch list for the whole read. Planet
`planet-20260223-with-indexdata`, `inspect --extended --bench 1`, one
unstored `--force` cell on `26db96fe` plus the
`pipeline_scratch_pool_capacity_peak_bytes` counter (30 decode workers,
25.2 average cores):

| Counter or measure | Value |
|---|---:|
| `pipeline_scratch_pool_capacity_peak_bytes` (one read's workers summed) | 33,088,768 B (31.6 MiB) |
| `pipeline_scratch_st_capacity_peak_bytes` (one worker) | 2,097,152 B |
| `pipeline_scratch_gr_capacity_peak_bytes` (one worker) | 4,096 B |
| Peak anon | 283,696 kB (277 MiB) |
| Wall | 27.09 s |

The pool sum is allocated capacity, 11 % of the read's peak anon at most
(an upper bound on what is resident). This run's scan is a projection,
which reads the tables from scratch, so building the tables straight into
the payload tail would remove none of it; the reasoning for not building
that is at the thread-locals in `src/read/pipeline.rs`. The wall is in
line with the stored 26.80 s cell `5b8e6c29` (`179f1b7e`, not a matched
pair); the counters record only when a list grows, a few dozen times per
worker at most, instead of on every blob.

## Payloads validated where they lie (bygg, 2026-11-02, against `7feb4370`)

`check --locations` validated every blob by copying the worker's
decompressed payload into a fresh `Vec` and building a block from it
(memcpy, allocation, the table append's regrowth, a free), only for the
verdict. It now runs the validator on the borrowed bytes
(`read::block::validate_in_place`). Planet
`planet-20260223-altw-keep-untagged`, `brokkr check-locations --variant
altw-keep-untagged --bench 1`, both arms built in the main tree (HEAD
cells stored, change cells unstored `--force`), arm order alternated:

| Pair | HEAD | Change |
|---|---:|---:|
| 1 (HEAD first) | 208.1 s (`586c7a63`) | 200.5 s |
| 2 (change first) | 207.4 s (`13ddbf33`) | 201.9 s |
| 3 (HEAD first) | 206.7 s (`c17025ee`) | 206.6 s |

Neutral within noise: the medians differ by 2.6 %, pair 3 by nothing,
and in pair 3 the node pass (the phase every blob is validated in) is
19.9 s at 22.4 to 22.6 cores in both arms. Peak anon about 18.2 GB in
both, set by the partition store. The work removed is one payload
memcpy and allocation per blob, a few core-seconds against roughly
4,500 core-seconds per run, so no visible wall delta was expected. The
change landed as a simplification, not as a speedup: payload copies of
this kind (a borrowed decompressed payload copied into a block that is
built only to be validated or read once) are not worth hunting for wall
time, here or at the other sites.
Partition 1 is the run's noisiest phase: in some cells of either arm it
takes 3 million major faults and about 15 s longer, with every phase
after it unchanged.

multi-extract's bbox-only node classify now runs one projection pool
over every node blob (a passthrough candidate's validated bytes read
for its dense-only check before the block is built) instead of a block
pool for the rest and a raw pool that copied each candidate. Planet
default 5-region simple config, main-tree builds: HEAD 88.5 s (control
cell, not stored), change 88.2 s and 87.9 s; node classify 19.4 s in
the HEAD cell and 18.9 s in the second change cell. That config nominates no candidate (no planet node blob's bbox
lies inside a region), so this measures the restructured pass, not the
removed copy; extract `--simple`'s candidate validation likewise never
runs on the planet bbox and was not measured.

## inspect --nodes reads through a projection (bygg, 2026-11-02, against `5ec5eefc`)

`inspect --nodes` reads each node blob through a projection on the block
validator (`NodeCoords`, on the accumulate pool's projection twin)
instead of building a `PrimitiveBlock` and walking every node: a dense
group's validated `lat` and `lon` arrays are decoded once the blob is
accepted, where the element walk stepped `id`, `lat`, `lon` and
`keys_vals` per node. Planet `planet-20260223-with-indexdata`, default
`-j`, interleaved `--bench 1`, both arms built in the main tree (HEAD
cells stored, change cells unstored `--force`), arm order alternated:

| Pair | HEAD | Change |
|---|---:|---:|
| 1 (change first) | 26.03 s (`2dcddb68`) | 22.46 s |
| 2 (HEAD first) | 25.98 s (`c4ad8414`) | 22.36 s |

-14 % wall. The scan phase (after the 6.6-6.8 s schedule walk both arms
share) went from 19.25 and 19.23 s at 25.3 cores to 15.60 and 15.71 s
at 21.7 cores, reading about 3.8 GB/s; peak anon 308 to 296 MB.

The same session measured the way passes that read only IDs and refs,
moved onto `read::project_ids::WaySpans` and kept as block walks, since
none moves its command past the noise floor (same arm protocol):

| Command | HEAD | Projection probe | Phase, HEAD to probe |
|---|---:|---:|---|
| tags-filter `w/highway=primary` | 75.72, 75.77 s (`2612d0fa`, `d7b43157`) | 73.81, 74.22 s | way-dependency walk after its schedule loop 10.27, 10.24 to 8.68, 8.74 s |
| extract `--smart`, Europe bbox | 132.05, 131.81 s (`abc90310`, `2b604dda`) | 131.94, 130.99 s | `PASS1_WAY_CLASSIFY` 16.78, 16.63 to 16.88, 16.85 s; `SMART_PASS2_CLASSIFY` 10.08, 10.06 to 8.64, 8.70 s |

A pass that tests a few way IDs against a set gains about 1.5 s, the
way-message re-parse a projection removes (2.3 % of tags-filter, under
1 % of smart). A pass that scans every way's refs against a node set
(smart and complete pass 1, multi-extract's way classify by the same
shape) gains nothing measurable: the ref lookups are its cost.

## inspect tags counts into a sharded table (bygg, 2026-11-02, against `a77eb163`)

Before the change, `inspect tags` built a `PrimitiveBlock` per blob, walked
every element resolving each tag's strings, and sent a per-blob
`HashMap<String, HashMap<String, u64>>` to the calling thread, which merged
it into the global map. At planet that single merging thread was the whole
wall: in the stored HEAD cell `4e2830c4` (default `-j`, 30 workers)
`classify_unordered_result_send_wait_ns` summed 1,856 s over an 88 s scan,
the workers parked about 70 % of the time, while the consumer's
`result_recv_wait_ns` was 0.3 s; an 18 s single-core tail followed
(reserving the result, walking and freeing about 10^8 strings). The element
walk itself was off the critical path.

The change reads each blob through a projection on the block validator,
counts (kind, key index, value index) per blob, resolves and hashes each
distinct pair once, and has every worker merge its blob into one table of
256 shards, a mutex each, shard by pair hash. A shard stores strings in
fixed 256 KiB chunks with keys interned per shard. Planet
`planet-20260223-with-indexdata`, default `-j`, interleaved `--bench 1`,
both arms built in the main tree (both unstored `--force`; the HEAD arm
by checking out the changed paths), arm order alternated:

| Pair | HEAD | Change |
|---|---:|---:|
| 1 (change first) | 98.91 s, peak anon 17.59 GB | 36.13 s, 10.82 GB |
| 2 (HEAD first) | 98.70 s, 17.59 GB | 36.25 s, 10.83 GB |

-63 % wall, -6.8 GB peak anon. After the 6.1 s schedule walk both arms
share, HEAD's scan and tail (no marker between them) took 92.67 and
92.46 s at 8.8 cores; the change's scan took 29.41 and 29.56 s at 23.4
cores, now CPU-bound, and its drain (`TAGSCOUNT_DRAIN`, which keeps no
rows under brokkr's `--min-count 999999999`) 0.67 and 0.59 s. The stored HEAD cell `4e2830c4`
earlier the same day ran 113.6 s because the host swapped (up to 4 GB,
1.3 M major faults) at its 16-17 GB peak; the change does not reach that
peak.

Table counters at the end of the scan: 187.4 M distinct pairs, 1.64 M
interned keys (107,879 distinct keys over 256 shards), 3.69 GB of strings
in 3.73 GB of chunks, 6.75 GB of hash tables. The first version stored the
key's bytes with every pair in one doubling vector per shard: 14.0 GB peak,
of which 1.86 GB was repeated key bytes and 2.7 GB the vectors' unfilled
reserve, at the same wall.

The figures above are a bare run counting every key=value pair, which is
what a bare `inspect tags` did until the change below; that workload is now
`inspect tags '*=*'`.

## inspect tags counts keys as osmium does (bygg, 2026-11-03, against `719ccf92`)

A bare `inspect tags` and an expression without `=` now count keys, one row
per key, as `osmium tags-count` does; an expression with `=` counts pairs.
brokkr's `inspect --tags` cells pass no expression, so the same brokkr
argv measures pair counting on cells up to `719ccf92` and key counting after
it. Planet `planet-20260223-with-indexdata`, default `-j`, both arms built in
the main tree.

Bare run (brokkr `inspect --tags --bench 1`, `--min-count 999999999`):

| Arm | Wall | Peak anon | Distinct rows |
|---|---:|---:|---:|
| HEAD, pairs (stored `97bd775a`) | 41.35 s | 10.26 GB | 187.4 M pairs |
| Change, keys (`--force`, unstored) | 29.97 s | 0.40 GB | 107,879 keys |

The key run tallies each blob per (kind, key index) instead of per (kind,
key index, value index) and holds 3.3 MB of table: its scan took 23.0 s at
21.7 cores against HEAD's 33.6 s at 22.3. The HEAD cell's scan read part of
the planet from disk (326 K major faults), so its wall is above the 36 s the
same code measured on 2026-11-02; the pair figures below are the matched
comparison.

Pair counting (HEAD bare against the change's `'*=*'`, the same 187.4 M
pairs; `brokkr run --release`, wall from `brokkr history`, order change,
HEAD, HEAD, change): HEAD 36.5 s and 36.5 s, change 36.9 s and 37.7 s. The
2 % median difference is inside the noise band; the change adds one
expression test per distinct pair per blob (`*=*`) and a wider pending
entry.

A bare run with the default `--min-count 1` now builds about 108 K rows at
planet instead of 187 M, so the row build and serial sort that were never
measured for the pair default are no longer a bare run's concern; `'*=*'`
with the default thresholds still builds every pair as a row.

## CLI commands against osmium, cold cache (bygg, 2026-11-08, at `b355badd`)

The guide's "CLI command benchmarks" table, re-measured: until this run its
pbfhogg column came from `6fc1283` cells on plantasjen and its osmium column
from February 2026 runs of an earlier script, and its inspect tags row timed
pair counting against osmium's key counting. Script bench `vs-osmium`
(`brokkr.d/benches.toml`), denmark `pbf.indexed`
(`denmark-20260220-seq4704-with-indexdata.osm.pbf`) read by both tools,
apply-changes against `osc.4705`, osmium 1.19.1 / libosmium 2.23.1.
`scripts/vs-osmium.sh 3`: per row three pairs of `--iterations 1`
invocations, order pbfhogg-osmium, osmium-pbfhogg, pbfhogg-osmium. Runs are
stored in brokkr's bench store, marked dirty only because the bench files
were uncommitted (the binary is HEAD's).

Cache state decides the comparison. libosmium advises its input out of the
page cache as it reads (`remove_buffered_pages`, `POSIX_FADV_DONTNEED`, on by
default), so a first campaign without eviction had pbfhogg slow after every
osmium run and fast after a pbfhogg run, by about 0.6 s on the light rows
(inspect 636 ms against 5 and 4 ms; getid 648 and 651 against 14 ms;
inspect tags --type way 690 and 696 against 49 ms), osmium flat. That
campaign is diagnostic only. The bench's preflight now drops each arm's
inputs (`dd iflag=nocache count=0`) and fails unless `fincore` finds none
resident, so every run below starts cold.

| Row | pbfhogg runs | osmium runs | Medians | Ratio |
|---|---|---|---|---:|
| sort, already sorted | 1.1, 1.0, 1.0 s | 7.5, 7.6, 7.5 s | 1.0 / 7.5 s | 7.5x |
| apply-changes | 785, 756, 774 ms | 4.4, 4.4, 4.5 s | 0.77 / 4.4 s | 5.7x |
| tags-filter -R w/highway=primary | 161, 164, 167 ms | 370, 361, 373 ms | 164 / 370 ms | 2.3x |
| tags-filter -R amenity=restaurant | 224, 227, 233 ms | 770, 815, 818 ms | 227 / 815 ms | 3.6x |
| cat --type way | 171, 162, 175 ms | 1.5 s x3 | 171 ms / 1.5 s | 8.8x |
| inspect tags, bare | 784, 769, 774 ms | 1.0 s x3 | 774 ms / 1.0 s | 1.3x |
| inspect tags --type way | 697, 689, 687 ms | 407, 392, 393 ms | 689 / 393 ms | osmium 1.8x |
| getid, 9 IDs | 644, 645, 646 ms | 589, 592, 581 ms | 645 / 589 ms | osmium 1.1x |
| add-locations-to-ways sparse | 1.9, 1.9, 2.0 s | 7.4, 7.5, 7.3 s | 1.9 / 7.4 s | 3.9x |
| add-locations-to-ways external | 2.5 s x3 | 7.3, 7.4, 7.4 s | 2.5 / 7.4 s | 3.0x |
| inspect | 638, 638, 639 ms | none | 638 ms | - |

Last-campaign run UUIDs (pbfhogg, osmium, in run order): sort `e4c201d9`
`44219ebb` `875dd38c` `b75e9538` `b72d7f0b` `01c875f2`; inspect tags --type
way `f1191e3a` `81cfc999` `373fa2c3` `83338ccf` `c4a028a7` `8433d981`;
inspect `1ef7bd64` `46c5d8da` `fdf00786`; the rest are in `brokkr results`
under bench `vs-osmium`.

The rows that answer from indexdata pay a cold header walk. Sidecar of
`f1191e3a` (inspect tags --type way): `SCHEDULE_SCAN_LOOP` 633 ms reading
47 MB with 5.8 K voluntary context switches and no measurable CPU, then 61 ms
of decode; bare inspect (`1ef7bd64`) is the same 637 ms walk alone, 56 MB
read. osmium's tags-count -t way (`81cfc999`) reads all 475 MB in 407 ms at
10.8 cores. The walk is latency-bound on one thread, a synchronous read per
blob, so on a cold cache it costs more than a sequential read of the whole
file. Warm, the same walk is the 5 to 49 ms above. The next section is the
fix; the inspect, inspect tags --type way and getid rows of the guide come
from it.

## Header walk read-ahead on cold dense files (bygg, 2026-11-08, baseline `bb414882`)

`PreadSource` (`src/read/header_walker.rs`) keeps its read window across
payload skips and, on a refill, reads a 1 MiB window instead of the 4 KiB
probe when two gates pass: density (mean of the last 8 completed frames
below 128 KiB, so the planet dump's ~1.8 MB frames never qualify) and
residency (a 4 KiB `preadv2(RWF_NOWAIT)` probe first; a hit is the window
at no I/O, and only 4 dense refills missing in a row authorise a window).
The walk is a dependency chain, so read-ahead is the only lever: in-flight
exact probes cannot overlap it. Variant cells are `bb414882` plus the
uncommitted change (bench store marks them dirty); baseline cells are
`--commit bb414882`. Every `vs-osmium` cell starts with its input evicted.

Density gate alone (first variant), alternating pairs:

| Row | Baseline | Variant |
|---|---|---|
| denmark inspect | 644 (`e7be7f03`), 638 (`e009a69f`) ms | 227 (`c108e83b`), 190 (`24f225ca`) ms |
| denmark inspect tags --type way | 693 (`9a53c4ee`), 691 (`92a2efbd`) ms | 263 (`b5f2b35b`), 240 (`c4cf854d`) ms |
| denmark getid | 650 (`2fb09f94`), 649 (`6e3c7fae`) ms | 198 (`6e0f1abd`), 199 (`f274ac24`) ms |
| planet inspect | 6.1 (`8d48e999`), 6.8 (`eb1cb944`) s | 6.8 (`c0c93714`), 6.1 (`6b01c0bb`) s |
| europe inspect | 45.6 s (`1f4345fc`) | 14.4 s (`1520f7b2`) |

Planet is unchanged: both arms read 654-659 MB and the second cell of each
pair was the faster one, an order effect. But density alone regressed a
header-warm walk: europe `check --refs` after a cold europe inspect had left
the headers cached and the bodies not, baseline `3dee9165` 8.3 s with
`SCHEDULE_SCAN_LOOP` 270 ms and no disk read, variant 27.6 s with the walk at
19.8 s reading 15.9 GB - windows fired at refills whose headers were
resident and read the bodies between them, and on a file larger than RAM
(35 GB against 29.8 GB) every walk of a multi-walk command would repeat it.
That is what the residency gate is for.

Final shape (both gates):

| Row | Cells | Median |
|---|---|---|
| denmark inspect | 224, 208, 217 ms | 217 ms (baseline 638-644 ms) |
| denmark inspect tags --type way | 266, 259, 264 ms | 264 ms (baseline 691-693 ms; osmium 395 ms, `7e13294a`) |
| denmark getid | 217, 216, 194 ms | 216 ms (baseline 649-650 ms; osmium 619 ms, `ac1e2007`) |
| europe inspect | 14.4 s (`445a6c9a`) | baseline 45.5-45.6 s (`1f4345fc`, `cb4aad64`) |
| planet inspect | 6.8 s (`8fdf1ee1`) | inside the baseline's 6.1-6.8 s |
| europe check --refs, headers cached | 8.24 s, walk 288 ms, no disk read | baseline 8.28 s (`3dee9165`), walk 270 ms |

The header-warm check-refs cell followed the same predecessor as its
baseline (a cold baseline europe inspect, `cb4aad64`); its counters were
464,510 cache hits, no misses, no windows. A warm denmark inspect is still
5 ms. Not measured: a header-warm walk under memory pressure with scattered
evicted headers (the unit tests pin that isolated misses read no window,
not its wall), and the multi-walk commands on a cold europe.

## Sort across the blob-merge engine move on unsorted Denmark (bygg, 2026-11-08)

`brokkr sort --dataset denmark --snapshot KEY --bench 1 --commit REF`,
arms `dd312160` (before the move), `79d2e923` (the move: sort and
`cat --dedupe` on one engine, overlap runs streamed from worker threads)
and HEAD `d044980e`, all three `--commit` worktree builds, kernel 7.2.14,
buffered, no fstrim. Three rounds per input with the arm order rotated
(old, move, HEAD; move, HEAD, old; HEAD, old, move). Walls in round order:

| input | blobs, overlap runs (old / move / HEAD) | `dd312160` | `79d2e923` | HEAD | peak anon old / move / HEAD |
|---|---|---|---|---|---|
| `unsorted` | 7396; 3 runs over 6 blobs | 2.55, 2.08, 2.03 s | 2.54, 2.02, 2.08 s | 2.21, 2.03, 1.98 s | - |
| `twin` | 14792; 3 / 7396 / 7396 | 30.6, 29.6, 29.2 s | 6.03, 6.08, 6.01 s | 4.94, 4.96, 4.95 s | 2354 / 791 / 200 MB |
| `chained` | 12487; 3 / 3 / 3 | 31.0, 30.2, 30.3 s | 37.7, 37.6, 37.5 s | 27.3, 27.8, 27.3 s | 2376 / 124 / 46 MB |

UUIDs, in table order. `unsorted`: `1d43e776` `a54557c7` `37ff193a`,
`d30735cf` `92c587ab` `b64a2d8e`, `84a25a94` `fe4316a5` `6e597f1d`.
`twin`: `9ed3f88d` `4e61665b` `33f33c3e`, `74cc5d63` `3357c694` `a2d87928`,
`97141886` `f15dda22` `637079d4`. `chained`: `1230eb36` `73ac97ab`
`d3b85feb`, `720206af` `e9be4e44` `8db6623d`, `d261fa4a` `6ce0b853`
`5ceb86fb`. Peak anon from `brokkr sidecar --stat anon` on `a2d87928`,
`74cc5d63`, `3357c694` (twin) and `8db6623d`, `720206af`, `e9be4e44`
(chained).

The inputs, all with every blob indexed and no sorted claim:

- `unsorted` is `degrade --unsort` of Denmark: one adjacent element pair
  swapped per kind, so at most three two-blob overlap components on this
  otherwise sorted source. It measures pass 1's decode of every payload
  (an unclaimed header) and passthrough; the overlap work is tens of
  milliseconds. Flat across all three arms; the first round's two cells
  ran before the input was warm.
- `twin` is `pbfhogg cat` of the Denmark indexed file with itself: every
  blob overlaps its twin. The old engine grouped adjacent rewrite blobs of
  one kind into a run, so it swept three kind-wide runs and collected
  their output before writing; the move sweeps one run per component (7396)
  and streams. The table measures that whole change, partition included,
  not scheduler overhead on equal jobs.
- `chained` is `pbfhogg cat` of two grow repacks of Denmark at
  `--elements-per-blob 9001` and `10007` (registered as snapshots
  `grow-9001` and `grow-10007`): with no shared interior blob boundary,
  each kind is one connected component, three runs on every arm. Pass 1
  is equal (`SORT_INDEX_BUILD` about 3.65 s); `SORT_PASS2` is 26.5 s old,
  33.9 s move, 24.1 s HEAD (`8db6623d`, `720206af`, `e9be4e44`). The move
  regressed this shape by about 28 % of pass 2 and HEAD is about 9 %
  faster than the old engine. The cause of the regression and of the
  recovery is not isolated (scheduling, buffering, positional reads and
  write overlap changed together at the move; the borrowed-cursor sweep of
  `13ec0927` is a plausible contributor to the recovery, not an
  attribution), and with HEAD ahead of both arms no bisect is warranted.

Every arm keeps both copies of a duplicated element (HEAD's twin output:
104,979,306 nodes, 13,233,052 ways, 92,206 relations, twice Denmark);
output bytes differ legitimately across the move, which flushes a block
at each component boundary. Element contents were not compared across
arms here; `brokkr verify sort` is the correctness check.

Pass 2 runs one sweep per component, so parallelism on unsorted input is
bounded by the component count: on `chained` three sweeps cover the whole
file and the node component dominates.

## sort --force on unindexed Europe (bygg, 2026-11-08)

`060c2f74` made sort's pass 1 scan each unindexed blob's tag keys along
with its IDs and hold the serialized key set to pass 2, so a reframed
passthrough carries tagdata as plain `cat` writes it. Measured
`060c2f74^` (`9744a9fc`) against `060c2f74` with the `sort-cold` script
bench (`benches/sort/sort-cold.lua`, `--force yes`), both arms `--commit`
worktrees, alternated A B B A, one iteration per invocation, input
evicted and checked 0 B resident by fincore. Input: europe `pbf.raw`
(`europe-20260301-seq4714.osm.pbf`, 31.6 GB, 522,168 blobs, header
claims `Sort.Type_then_ID`, no indexdata). Every arm: all 522,168 blobs
passthrough, none overlapping, rewritten or restructured.

| Arm | Run | SORT_INDEX_BUILD | Pass 2 | Child wall | Peak anon | Output bytes |
|---|---|---|---|---|---|---|
| `9744a9fc` | `7eeed226` | 164.66 s | 14.97 s | 179.6 s | 59.7 MB | 33,981,195,365 |
| `060c2f74` | `35892bd8` | 167.63 s | 19.24 s | 186.9 s | 954.6 MB | 34,858,236,257 |
| `060c2f74` | `224bd93a` | 165.12 s | 15.27 s | 180.5 s | 956.1 MB | 34,858,236,257 |
| `9744a9fc` | `ec335d61` | 161.57 s | 15.03 s | 176.6 s | 58.8 MB | 33,981,195,365 |
| `fc076a09` (HEAD) | `13828c41` | 170.64 s | 15.30 s | 186.0 s | 954.6 MB | - |

- Index build: median 163.1 s against 166.4 s, +2.0 %, the noise floor.
  Pass 1 runs at 0.8 cores in both arms: the payload scan was already
  serial before `060c2f74`, which added the tag-key marking and
  serialization to it, not the serial read. The HEAD cell is one sample
  and sits inside the band.
- Peak anon: +~895 MB, matching the 877 MB of tagdata the output gained
  (about 1.7 KB per blob). The key sets live from pass 1 until the plan is
  dropped after pass 2; `plan`'s four per-frame flag vectors are planning
  scratch (about 2 MB here) and are gone before pass 2.
- Verdict: no wall cost; a memory cost equal to the output's tagdata,
  with no fixed bound (it scales with reframed passthrough content, so the
  Europe ratio of 2.6 % of input is an observation, not an envelope).
  Accepted on this path; the rationale and the spill remedy are at
  `Reindex` in `src/commands/blob_merge.rs`. Planet `pbf.raw` (50,816
  blobs) was not measured.

### Pass 1 scans on a worker pool (bygg, 2026-11-09, against `252ad27a`)

Pass 1's payload scans (pread, decompress, ID and tag-key scan, the
reframing decision) moved to an ordered worker pool; the header walk and
the in-order fold stay on the calling thread, and believed blobs travel
in batches with no payload read. Same `sort-cold` bench and input as
above; baseline `--commit 252ad27a` worktree, variant the uncommitted
working tree (`--force` runs, numbers from the run output and sidecar).

| Arm | Run | SORT_INDEX_BUILD | Pass 2 | Child wall | Peak anon | Output bytes |
|---|---|---|---|---|---|---|
| variant | `663943f9` | 14.62 s | 21.68 s | 36.3 s | - | 34,858,236,257 |
| `252ad27a` | `19a8bdfa` | 164.75 s | 15.10 s | 179.9 s | 949 MB | - |
| variant | `e3e28886` | 13.56 s | 11.93 s | 25.5 s | 1,073 MB | 34,858,236,257 |

- Index build: 164.7 s to about 14 s (11x); the sort's wall 180 s to
  26-36 s. Pass 2 is untouched; its spread across the variant cells is
  page-cache carry-over from pass 1, not code.
- Peak anon: about +125 MB, the workers' read and decompression scratch.
- The believed path (indexed europe, `brokkr sort --bench 1`, header-only
  walk), alternated variant / base / variant / base: SORT_INDEX_BUILD
  14.0 / 7.0 / 8.6 / 10.3 s (`cc4fd3d7`, `a5dabcf8`, `8d2bb173`,
  `c5fd3184`). The signs disagree across the pairs, so no cost is shown;
  that walk is I/O bound and swings with cache state in both arms.

## Pipelined writer delivery window (bygg, 2026-10-31)

The pipelined writer admits each item against a delivery window
(`PIPELINE_DELIVERY_WINDOW`, 512 items, returned at ordered handoff).
Before it, finished items behind a slow head were limited only by the
reorder buffer's failure ceiling. Planet, interleaved `--bench 1`, no
fstrim beforehand. Baseline cells are `8aff105a`; variant cells are the
same commit plus the uncommitted change, run with `--force`, so they have
no UUID.

| Command | Cell | Wall | Window wait | Reorder span / filled |
|---|---|---:|---:|---:|
| apply-changes 4913 zstd:1 | `b9c4df3a` base | 126.6 s | | |
| | variant | 128.9 s | 0.06 s | 112 / 112 |
| | `aeaf92e5` base | 141.3 s | | |
| | variant | 128.5 s | | |
| altw external | `115c9b21` base | 437.7 s | | |
| | variant | 455.8 s | 0.012 s | 204 / 181 |
| | `361e31e7` base | 460.5 s | | |
| | variant | 458.5 s | 0.010 s | |

The deltas change sign between pairs, and the producer waited on the
window for a total of 10 to 60 ms per run, so the window costs nothing
measurable on normal input. Raw-send-only commands (cat --clean,
time-filter, getparents) have a reorder span of 1 and cannot reach it.
It does not shrink these runs' peaks: their pile-ups (a reorder span in
the low hundreds, about 25 MB of 146 KB raw items on apply-changes and
about 270 MB of 1.3 MB frames on altw external) sit below the window. It
replaces the unbounded worst case behind a slow head with backpressure.

### Reorder buffer bytes (bygg, 2026-11-04)

`writer_reorder_bytes_high_water` reports the peak output-buffer capacity
held in the pipelined writer's occupied reorder slots, sampled with the
span and filled peaks (definition in `src/write/metrics.rs`). Planet,
interleaved `--bench 1` after `fstrim`. Baseline cells are `184fe3d6`;
variant cells are that commit plus the uncommitted counter, run with
`--force`, so they have no UUID.

| Command | Cell | Wall | Reorder span / filled | Reorder bytes |
|---|---|---:|---:|---:|
| cat --clean | variant | 144.6 s | 1 / 1 | 2.1 MB |
| | `612ed3eb` base | 145.9 s | | |
| altw external | variant | 327.7 s | 231 / 231 | 466 MB |
| | `467a50a9` base | 343.8 s | | |

The variant is not slower in either pair; the altw delta sits in stage 1's
relation scan, before the writer receives anything, so it is drive and
cache state, not the counter. The byte peak is about 2.0 MB per slot,
above the 1.3 MB mean frame, so the earlier slots-times-mean estimate of
about 270 MB undercounted what altw external holds there. cat --clean and
apply-changes now send pre-framed raw items in order from the producer
(`writer_payload_framed_items` 0), so their reorder buffer holds one item
at a time and the counter reads the largest single item.

## Node boundary on the drain channel, not measured (2026-11-09, at `a5a9ba08`)

`ff0b0a37` moved the `--locations-on-ways` node boundary from its own
one-slot channel, which the drain polled with `try_recv` on every loop
iteration, onto the shared drain channel as `DrainMessage::NodeBoundary`.
The drain now checks once after each receive and contiguous advance. The
planned planet pair, `ff0b0a37^` against `ff0b0a37`, was not run. This
is an argument from the source, not a measurement:

- Publication becomes ready once the boundary has been received and every
  node seq below it is processed, in both arms. Before publication no way
  or relation item exists, so every message queued ahead of the boundary
  is node work the drain must process anyway. The cost is one extra
  control-message receive. When the boundary is late, the new shape
  publishes as soon as it is received, where the old one waited up to one
  `DRAIN_RECV_TIMEOUT` to poll for it. This is not a numerical bound on
  wall time.
- At HEAD the scanner sends the boundary and then blocks on the barrier
  signal (scanner.rs, `send_boundary` then `wait_for_barrier`).
  Publication needs the boundary to have been received, so a send blocked
  on a full channel finishes inside a barrier wait that happens anyway.
  It adds no delay to the scanner.
- At the pair the scanner went on to walk the way and relation headers
  after sending the boundary, and under `-l` every node descriptor went
  through the workers, so the boundary was the scanner's first send on
  the drain channel. A blocked send could therefore lose overlap between
  that tail walk and the remaining node work. That risk is unquantified,
  and the tail walk itself was removed in `0f417a44`. The pair's verdict
  would not describe HEAD.
- Wrapping each item in the `DrainMessage` enum costs something on every
  item. It is unmeasured and judged too small to show in a planet wall.

The same goes for the ride-along `20d10610`, which moved the OSC
interner's `resolve` from `from_utf8(..).unwrap_or("")` to `str::get` on
the range. That drops validation proportional to string length, once per
resolved key, role and user name; tag values are still checked with
`from_utf8`. It is an ancestor of both arms of the pair above, so that
pair could never have measured it. It has no cell of its own.

## Apply-changes sequence window (bygg, 2026-10-31)

The apply-changes scanner admits each blob seq against a window
(`APPLY_CHANGES_SEQ_WINDOW`, 512), returned when the drain processes the
seq, so the drain's reorder map is bounded. Planet, interleaved
`--bench 1`, no fstrim beforehand. Baseline cells are `26fe5a5e`; variant
cells are that commit plus the uncommitted change, run with `--force`, so
they have no UUID.

| Command | Cell | Wall | Window wait | Seq window high water |
|---|---|---:|---:|---:|
| apply-changes 4913 zstd:1 | `188db700` base | 126.9 s | | |
| | variant | 144.8 s | 0 | 412 |
| | `be9267e5` base | 139.7 s | | |
| | variant | 131.6 s | | |
| | `f4cf4f4c` base | 136.4 s | | |
| | variant | 135.4 s | | |
| apply-changes 4913 (zlib) | `f62acf33` base | 240.9 s | | |
| | variant | 242.4 s | 0 | 321 |
| | `f710f3ce` base | 240.5 s | | |
| | variant | 240.9 s | | |
| apply-changes `-l`, altw-keep-untagged | `18118dbf` base | 314.1 s | | |
| | variant | 317.0 s | 0 | 298 |
| | `32518511` base | 314.8 s | | |
| | variant | 314.7 s | | |

The scanner never waited on the window, so on this input it changes no
scheduling; the zstd:1 cells swing between about 127 s and 145 s on both
arms (the same bimodality as the delivery-window pairs above), and the
medians agree (136.4 s base, 135.4 s variant). In-flight seqs peak at
300-410, above the reorder map's 150-190 because the map is only part of
what is in flight; a window under about 420 would start to bind at planet.
The window caps the worst case, not these runs' peak: the reorder map
still reaches 174 items / 415 MB on the first zstd:1 variant cell.

## Apply-changes overlay: superseded records (bygg, 2026-10-31)

Measured at `3f8d9f35` with temporary per-record length counters in the
overlay's push and delete paths (unstored `--force` run stopped at
`MERGE_LOOP_START`), planet daily diff seq 4913:

| kind | records appended | bytes appended | superseded by a later upsert | orphaned by a later delete |
|---|---:|---:|---:|---:|
| node | 2,917,899 | 168.0 MB | 40,540 / 2.9 MB | 60,905 / 4.9 MB |
| way | 547,987 | 119.6 MB | 21,452 / 8.2 MB | 907 / 0.24 MB |
| relation | 17,326 | 38.0 MB | 1,812 / 8.7 MB | 49 / 0.01 MB |

About 25 MB of 325.7 MB appended arena payload is unreachable. The 575 MB
`merge_diff_heap_bytes` of that commit was `heap_size_estimate`: arena
capacity (growth headroom included, much of it untouched pages), hash
tables, deleted sets, floors and the interner, so it was not the resident
overlay (the next section counts each part). Across a week,
`merge-changes --simplify` over seq 4914-4920 (cell `0df5c0ad`, `44c62853`)
reduced 26,261,540 changes to 25,417,990, 3.2 % by count. Compacting the
overlay was declined on these numbers; the reasoning sits on
`CompactDiffOverlay` in `src/osc/parse.rs`.

## Apply-changes retained lookup storage (bygg, 2026-11-02)

What apply-changes held beside the arena records for the whole run, counted
at `b5680edb` with temporary counters (exact allocation sizes from `Vec`
capacities and hashbrown bucket counts, unstored `--force` run stopped at
`MERGE_LOOP_START`), planet daily diff seq 4913:

| part | entries | bytes |
|---|---:|---:|
| arenas, appended (capacity) | | 325.7 MB (469.8 MB) |
| live hash indexes | 2,816,454 / 525,628 / 15,465 | 89.7 MB |
| deleted sets | 771,375 / 50,447 / 540 | 10.0 MB |
| version floors | 822,482 | 19.0 MB |
| interner lookup map and its keys | 13,692 strings | 1.0 MB |
| interner strings and table | 168 KB of strings | 0.4 MB |
| `DiffRanges` (six sorted ID vectors) | | 60.3 MB |

The strings only deletes interned (user names) were 48, 537 bytes. glibc
agreed (`mallinfo2` after the ID ranges: 610 MB in mapped chunks plus 40 MB
in the heap). After the parse nothing reads the hash indexes, deleted
sets, floors or lookup map except through lookups the sorted vectors could
answer, so apply-changes now turns the overlay into `SortedDiff`
(`src/osc/sorted.rs`): per kind sorted upsert IDs, parallel record
offsets and sorted deletes, 60 MB, the same as `DiffRanges` alone. Retained
allocation 650 MB to 530 MB (`merge_diff_heap_bytes` 574,583,740 estimated
to 530,454,912 exact); anon at the end of setup about 570 MB to about
450 MB. The workers' per-element hash probes (deleted set, then index)
became forward cursors over the sorted lists.

Planet, `--osc-seq 4913`, interleaved `--bench 1`, `sudo fstrim -av` first.
Both arms are unstored `--force` main-tree builds, base `b5680edb` and
variant that commit plus the change, swapped by checkout, order
alternated. Core-seconds are `merge_streaming_rewrite_ns` and
`merge_streaming_precise_ns`.

| cell | wall | peak anon | rewrite | precise |
|---|---:|---:|---:|---:|
| zstd:1 variant | 88.6 s | 2,470 MB | | |
| zstd:1 base | 95.8 s | 2,408 MB | 1,295 | 85.8 |
| zstd:1 base | 100.8 s | 2,584 MB | 1,265 | 82.8 |
| zstd:1 variant | 89.1 s | 2,288 MB | 1,046 | 100.7 |
| zstd:1 variant | 95.7 s | 2,974 MB | 990 | 95.1 |
| zstd:1 base | 97.6 s | 2,687 MB | 1,290 | 85.1 |
| `-l` altw-keep-untagged base | 269.9 s | 3,521 MB | 1,552 | 137.7 |
| `-l` altw-keep-untagged variant | 261.4 s | 3,317 MB | 1,260 | 162.9 |

zstd:1 medians 97.6 s base, 89.1 s variant, faster in every pair; the
rewrite loses about 250 core-seconds of hash probes, the precise check
gains about 12 (two cursors per element, upserts and deletes, where one
merged vector served). Peak anon is set by the run-to-run variable
backlog in the drain channel (later removed, "apply-changes' drain channel
held the node phase's backlog"), wider than the 120 MB this removes. Under
`-l` the setup phase went 6.1 s to 5.5 s (the referenced-node lookup is a
merge join over sorted refs), one pair.

## `--direct-io` impact summary

| Workload | Bottleneck | `--direct-io` effect |
|----------|------------|---------------------|
| Merge (NA, 18.8 GB) | I/O (concurrent read+write) | **-20%** (uring+none) |
| Merge (Germany, 4.5 GB) | Mixed | Neutral |
| Cat passthrough (planet) | Sequential I/O | +5% slower |
| ALTW (europe) | Memory latency (mmap faults) | +2% slower |
| `cat --dedupe` self-merge (planet) | CPU (sweep), every frame read twice | +16% slower with a direct pass 2 (see below) |

`cat --dedupe` direct pass 2 (bygg, planet self-merge, 2026-10-30, change
on top of `83c3cce1`). With `--direct-io`, HEAD (pass 2 buffered) cell
`ef8f68ee` 265.6 s against an uncommitted direct-pass-2 cell 306.9 s: the
write loop reads 175.7 GB from the device instead of 86.5 GB, because the
self-merge reads each frame once per argument and a direct read gets no
page-cache reuse; average cores 28.1 vs 27.5. The cost is the requested
transport on a doubled-input benchmark shape. The buffered default path
(one branch per frame read) was checked with interleaved `--bench 1`
cells: HEAD `d83a5389` 304.9 s and `1afa4e81` 297.7 s against uncommitted
261.1 s and 259.4 s, identical device reads (86.6 vs 86.5 GB in the write
loop) and pass 1; the gap sits in the CPU-bound write loop and the later
HEAD direct-io cell `ef8f68ee` ran that same buffered pass 2 in 251.7 s,
so it is drift between cells, not the change, and is not claimed.

`sort --direct-io --io-uring` with direct pass-2 copies (bygg, planet,
sorted input, 2026-11-04, change on top of `bc075247`). The copied-through
frames are now read `O_DIRECT` in aligned chunks into the uring writer's
buffers instead of through a second, buffered descriptor. Three
interleaved `--bench 1` pairs, HEAD stored, change `--force`: HEAD
`b9d61181` 45.0 s then change 53.6 s; HEAD `8ed88f79` 57.2 s then change
58.5 s; change 60.6 s then HEAD `fb083b98` 57.1 s. The first pair is the
first cell after a long rest and does not compare. The other two put the
change 2-6 % behind, a sign held across both orders. Pass 1 matches
(6.86 s each) and both write loops read 87.6 GB from the device, so the gap
is the read pattern of the write loop, not more bytes: synchronous direct
chunks get no kernel readahead. Inside the noise band for a single pair,
and the transport is the one `--direct-io` asks for.

`sort --io-uring` against the buffered default (bygg, planet, sorted
input, `bc075247`, 2026-11-04), interleaved stored `--bench 1` cells:
buffered `d18ccf43` 52.1 s, uring `4b51c917` 47.5 s, uring `ac69de9f`
55.8 s, buffered `15393169` 51.5 s. A tie on the median (51.7 s against
51.8 s), with the uring arm's spread wider. Sorted planet is a pure copy of
about 87 GB, and both write loops read it from the device at about
1.8 GB/s with well under one core busy (0.4 buffered, 0.1 uring), so the
drive bounds both arms and the write backend cannot move the wall.

`sort` across the blob-merge engine move, `dd312160` (old) against
`79d2e923` (new) (bygg, planet sorted reindexed input, kernel 7.2.14,
2026-11-08, both arms `--commit` worktrees, buffered, no fstrim: it was
refused in the session). Sequence after a discarded warm-up cell
`ee055efc` (new, 51.8 s, pass 2 44.9 s): old `4f654ece` 75.3 s, new
`b9f304ca` 88.4 s, new `e51b3f6a` 80.2 s, old `fdc99aeb` 78.8 s. Pass 1
(cold header walk) is 6.06-6.08 s in all four. Pass 2 is all of
`writer_write_ns`: old 69.2 / 72.7 s, new 82.3 / 74.2 s, so new minus old is
+13.1 s and +1.5 s in the two adjacent pairs. Counters match on both arms:
50816 blobs, 0 overlap runs, 0 rewritten, one copy-range item of
92010514684 bytes coalescing 50815 frames, 0 userspace-fallback ranges.
Both engines submit the same bulk copy through the same copy loop; the
timing is inconclusive, because every cell after the warm-up ran 50-80 %
slower than it under untrimmed back-to-back 92 GB writes, and the two pair
deltas differ ninefold. Not re-measure-worthy without a trimmed suite and a
reason in the code to expect a pass-2 difference.

## Plain cat across the one frame decoder (bygg, 2026-11-09, `2c12a955` vs `307982d7`)

`307982d7` moved the raw-frame walks from a zeroed `vec!` plus
`read_exact` to a capacity-sized `Vec` filled by a bounded `read_to_end`.
`FileReader` implements only `read`, so std's default `read_buf` still
zeroes every payload byte, and a zeroing reader keeps `read_to_end` at
8 KiB-and-doubling read sizes. Plain `brokkr cat --dataset planet --bench
1`, `planet-20260223-reindexed`, both arms `--commit` worktrees, buffered,
order A B B A, untrimmed:

| Arm | Wall | utime | read syscalls | minflt | peak RSS |
|---|---:|---:|---:|---:|---:|
| `2c12a955` (`e304c6b0`) | 42.7 s | 2.32 s | 101,646 | 435k | 80 MB |
| `307982d7` (`451494a3`) | 50.4 s | 2.95 s | 351,017 | 148k | 43 MB |
| `307982d7` (`a124a95e`) | 78.5 s | 3.17 s | 351,017 | 181k | 52 MB |
| `2c12a955` (`8b78f088`) | 84.6 s | 2.55 s | 101,646 | 583k | 79 MB |

The walls climb with every back-to-back 92 GB write and the pair deltas
change sign: no wall difference is detectable. The run is I/O bound
(0.2-0.4 cores, stime flat at about 15 s); the new walk costs about 4.9
more read syscalls and 12 us more user CPU per blob (0.6 s in all) on a
reader thread that is not saturated. Both arms zero the payload, so the
pair does not measure what removing the zeroing would gain.
`FileReader::read_buf` is not writable on stable Rust, and `resize` plus
`read_exact` keeps the zeroing. The pair covers neither
cat's descriptor admission (`56f8d9a9`, an ancestor of both arms, a
per-run cost) nor the reframe's `MAX_BLOB_DATASIZE` compare (`f9625b94`,
in neither arm, and plain cat on indexed input never reframes). Not to be
re-measured without a change to the walk's read path.

The zero-free append that followed (next section) is such a change and was
measured there.

## Zero-free FileReader append (bygg, 2026-11-09, `3286a9c5` vs uncommitted variant)

Every `Frames` built over a `FileReader` now appends through
`FileReader::append_to`: buffered bytes copied out of the `BufReader`, a
remainder of at least its capacity read by a bounded `read_to_end` on the
`File` (native `read_buf`, no zero-fill), O_DIRECT copied from its aligned
buffer. Baseline cells stored at `3286a9c5`; variant cells `--force`
(sidecar ids). Buffered, untrimmed, A B B A per command.

| Cell | Arm | Wall | utime | read syscalls |
|---|---|---:|---:|---:|
| planet plain cat | base (`dc7ccb05`) | 42.2 s | 2.96 s | 351,017 |
| planet plain cat | zero-free (`2605125d`) | 41.4 s | 0.45 s | 152,452 |
| planet plain cat | zero-free (`2179f0ea`) | 57.7 s | - | - |
| planet plain cat | base (`907f94f4`) | 59.0 s | - | - |
| europe getid full-scan arm | base (`5eb33820`) | 8.96 s | - | - |
| europe getid full-scan arm | zero-free (`66b28f09`) | 9.02 s | - | - |
| europe getid full-scan arm | zero-free (`a752d494`) | 8.95 s | 0.30 s | 134,052 |
| europe getid full-scan arm | base (`b983f104`) | 8.97 s | 0.44 s | 130,853 |

The mechanism shows (planet: about 3.9 fewer read syscalls and 50 us less
user CPU per blob; europe getid, 458k blobs under the 256 KiB buffer: a
third less user CPU, syscalls unchanged), the walls do not: both commands
are I/O bound (planet 0.4 cores with walls climbing per back-to-back 92 GB
write, europe getid 0.2 cores at about 3.9 GB/s from disk). The
warm-cache many-small-blob walk where the zeroing could reach the wall
cannot be built on this 30 GB host (europe and planet 8k exceed RAM).
Landed as a CPU reduction with no wall cost. Not to be re-measured
without a change to the append or a host that can hold such a walk warm.

## Writer cluster across six commits (bygg, 2026-11-09, `307982d7` vs `52690f1e`)

`307982d7..52690f1e` is six linear commits: one per-block dispatch
(`write_osm_data` over `BlobMeta`, one `reserve`, one `send_raw`), one
`WritePipeline` spawn, one checked frame header encoder
(`encode_frame_header`), writer counters that mean one thing on every
backend, one ordered-delivery loop (`deliver_in_order`) and the OSC gzip
writer reordering with `ReorderBuffer`. No single command reaches all of
it: repack and cat `--clean` frame on their own threads and send
`write_raw_owned`, so they never call `write_osm_data`. Three commands,
both arms `--commit` worktrees, order A B B A, `--bench 1`, untrimmed:

| Command | `307982d7` | `52690f1e` | `52690f1e` | `307982d7` |
|---|---:|---:|---:|---:|
| europe `repack` (raw send, framing, counters, loop) | 151.9 s (`5f0a1965`) | 141.0 s (`8de80460`) | 140.4 s (`99ce34ff`) | 140.9 s (`05e546f7`) |
| germany `extract --simple -b 5.8,47.2,15.1,55.1` (per-block dispatch) | 14.5 s (`2368455a`) | 13.0 s (`9e94ace5`) | 8.68 s (`ce19a942`) | 8.60 s (`ad0ae918`) |
| planet `diff-snapshots --format osc -j 16` (`ParallelGzipWriter`) | 257.4 s (`d673ef92`) | 256.0 s (`a5dd0067`) | 255.8 s (`2d9b1b50`) | 255.5 s (`d17d94db`) |

The germany extract walls fall as the cache warms; one more warm pair,
B then A, gave 8.70 s (`c8f486f7`) and 8.64 s (`5f34dc1a`), so warm B
is about 0.8 % slower: noise. Repack's first cell is the suite's first
and stands apart from the other three. The planet OSC assembly span
(`DERIVECHANGES_WRITE`, the `ParallelGzipWriter` stage) is 8.50 s at A
and 8.23 s at B (`d17d94db` against `2d9b1b50`). Verdict: no detected net
regression for these workloads on the buffered backend. It is a
regression screen, not a per-commit bound: a cost in one commit could
cancel a gain in another. Uncovered: the `pwrite` pool and io_uring
backends (io_uring cannot run on bygg) and the borrowed `BlobMeta::Scan`
and `Bare` branches. Repack at both arms runs under the CLI's two-arena
malloc cap (removed in `25172fc3`); extract and diff never asked for it.

Whole-Europe extract `--simple` is not a usable cell at either arm: both
panic with `reorder buffer window exceeded: seq 65536 is 65536 ahead of
0 (max 65536)`, and the Germany box on Europe panics the same way at
seq 117597, with blob 0 never delivered. Both arms fail identically, so
it is not the writer cluster. HEAD runs the same extract (`dfe4ec94`);
`56a50e0d`, which put every ordered blob pool on one bounded engine, is
the likely fix but was not bisected. Not to be re-measured without a code reason.

`--direct-io` only helps when page cache is a bottleneck (concurrent I/O on
files exceeding available RAM). Sequential reads and memory-bound workloads
are better served by buffered I/O with kernel readahead.

## Scheduled-read advice: RANDOM against plain (bygg, 2026-11-09, at `3d980bee`)

The header walker's description is advised `FADV_RANDOM`, and the pools a
walk hands it to read under that advice; the other pools open plain
descriptions. Neither choice had been measured. Variants were uncommitted
(`--force`, not stored); `read_ahead_kb` is 128 on bygg's nvme. Europe
`add-locations-to-ways --index-type external --bench 1`, consecutive cells:

| Arm | Wall | Stage 1 pass A | Relation scan | Stage 2 | `s2_node_pread_ns` sum | Stage 4 | `s4_pread_ms` sum |
|---|---:|---:|---:|---:|---:|---:|---:|
| HEAD, all plain (`36c31139`) | 112.3 s | 12.14 s | 4.55 s | 47.90 s | 22.3 s | 44.90 s | 64.6 s |
| stage 4 RANDOM (dirty `1f9fc4cb`) | 111.0 s | 12.16 s | 4.60 s | 46.93 s | 22.0 s | 44.63 s | 65.2 s |
| stages 1, 2, 4 and the relation scan RANDOM (dirty `9c63619a`) | 118.3 s | 12.30 s | 4.69 s | 53.05 s | 68.1 s | 45.66 s | n/a |

Stage 2 is the cost: each worker preads the contiguous run of node blobs
for its id bucket in file order (464,702 preads, 21.3 GB, identical in all
arms), a sequential stream that readahead pipelines and RANDOM turns into
synchronous demand reads. Stage 4 dispatches blobs one at a time across 30
workers, so no reader has a run and the advice does not matter there.

The classify side, extract `--simple -b=-25,34,45,72` on europe with the
three classify pools on a fresh RANDOM description against their usual
fresh plain one (WILLNEED unchanged; both arms dirty, order V B V B):
node classify 5.90 / 5.82 / 5.89 / 5.82 s, way classify 3.31 / 3.28 /
3.29 / 3.28 s, walls 51.7 / 51.1 / 51.2 s and B's second cell not run.
About 1 % against RANDOM, consistently, which is below noise.

Ruling: advice follows how a pool's readers walk the file. Readers of
contiguous runs keep a plain description (altw external stage 2,
measured). Per-blob dispatched pools are indifferent (stage 4, extract
simple's classify pools), so the walker handoff stays: it keeps the
scheduled file's identity for a path source at no measured cost.
Unmeasured: apply-changes' pool, renumber's pools, extract's
`pread_execute` (its consumer reads passthrough frames in schedule order)
and the relation scan in isolation; planet, where stage 4 is
readiness-bound (eb94af17: readiness waits sum 1622 s against pread
552 s). Not to be re-measured without a code reason.
