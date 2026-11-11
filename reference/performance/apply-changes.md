# Apply Changes

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

### Hardware gzip CRC is neutral (bygg, 2026-11-11, `9b76817b` + change)

zlib-rs with `std`, so gzip checksums use carry-less multiply (see
merge-changes.md, "Hardware gzip CRC"). After a trim, `--iterations 1`, both
arms `--commit`, A B B A, planet apply-diff of OSC 4913: `9b76817b` 191.6,
195.2 s (`da7362cb`, `054477fc`); with `std` 194.2, 194.2 s (`a573fdd4`,
`8bd4c062`). The 0.4 % between the means is noise.

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
[`crates/lib/src/write/parallel_writer.rs`](../../crates/lib/src/write/parallel_writer.rs); the
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
(`crates/lib/tests/apply_changes_create_runs.rs` pins the cuts).

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

### apply-changes keeps absent OSC metadata absent (bygg, 2026-11-10, `cb42cf0f` against `bd169b02`)

A node with partial OSC metadata is written as a plain Node message;
nodes with complete or no metadata stay dense, and a dense run now holds
only one of the two. Real diffs carry complete metadata, so the dense
path gains only an untaken branch.

Denmark apply-diff, `--iterations 1`, interleaved matched pairs after a
trim, both arms built with `--commit`: pair 1, 310 ms (`163df027`)
against 308 ms (`36db967b`); pair 2, 315 ms (`dd6fe479`) against 306 ms
(`cb4a0943`). Within noise: neutral, as expected. Denmark only; no planet
cell was run, since the dense hot path's work is unchanged.

### apply-changes' create-run loop lifted into a function its tests drive (bygg, 2026-11-04, against `3c5b8d3a`)

`CreateEncoder::emit`'s batching and ordered-write loop moved, unchanged,
into `write_run` (`crates/lib/src/commands/apply_changes/stream_output.rs`), which
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
`CompactDiffOverlay` in `crates/lib/src/osc/parse.rs`.

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
(`crates/lib/src/osc/sorted.rs`): per kind sorted upsert IDs, parallel record
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

## History

### Merge (apply-changes)

#### Descriptor-first streaming pipeline (P1 + P1.5 + parallel pwrite, 2026-04-21, commits `719f306` → `80b37df`)

Pre-flip baseline at commit `52c2c4b` (UUID `e81a9316`): 144.4 s. Best
post-flip 80.9 s (-44 %) at zstd:1 + cross-disk + parallel pwrite (parallel
pwrite was unaffected by the CopyRange bug, so the headline number is
unchanged). Same-disk best on the fixed writer: 104.5 s at zstd:1 + parallel
pwrite.

Memory + runtime shape at planet (buffered same-disk, `--compression none`,
commit `719f306`):

| Metric | Legacy `52c2c4b` | Post-flip `719f306` | Δ |
|---|---:|---:|---:|
| Wall | 144.4 s | 135.5 s | -6 % |
| Peak RSS | 1.63 GB | 3.29 GB | +2.0× |
| Peak threads | 27 | 50 | +85 % |
| Involuntary context switches (max sample) | 7,214 | 2,134 | **-70 %** |
| Major faults (max sample) | 52,659 | 67,858 | +29 % |

Per-worker thread-local `BlockBuilder` + scratches × 22 ≈ 220 MB,
per-worker coord slots, framed-chunk buffering at the drain (~800 KB
per rewrite blob in-flight), and the `BTreeMap<seq, DrainItem>`
reorder buffer account for most of the RSS delta.

Same axis points re-measured at europe scale on the fixed writer
(`16e3694`, 2026-04-26): same-disk io-uring at none / zlib:6 / zstd:1
= 57.9 s / 58.5 s / 53.9 s (UUIDs `377ac699` / `0d62d01a` / `42b24498`).
Original tainted values were 47.2 s / 55.3 s / 39.2 s at `6c9dbc7`
(UUIDs `36dee15a` / `5647f9fa` / `72413ff3`).

Every `--locations-on-ways` figure in this section ran on an `altw` base
made without `--keep-untagged-nodes`, so most OSC-way refs resolved to
nothing and were written as `(0, 0)`; those runs time a merge that did
little coordinate work, and the current code refuses them.

#### Inline-location refresh (2026-10-26, commit `81f3fbe4`)

`--locations-on-ways` used to splice way blobs the OSC did not touch,
leaving ways that use a moved node stale. The fix searches those blobs
(wire-level scan, clean ones still spliced) and refreshes surviving ways
in rewritten blobs. Before it landed, a read-only measurement
(`crates/lib/examples/stale_way_blobs.rs`) put the cost at about 5% more way blobs
rewritten at europe and 4% at planet, with detection, not recompression,
the work to manage. On europe the added worker time (about +137 s summed
over 22 workers: decompress, scan, rewrite, frame) was mostly offset by
seeding `block_overlaps_diff`'s cursors by binary search (-86 s precise
check), and interleaved walls on bygg did not separate from the drive
noise. Figures in `reference/performance/apply-changes.md` "Inline-location refresh".

Pool-size sweep at cross-disk zstd:1 (plantasjen, Samsung 990 PRO):
4 → 89.2 s, 8 → 83.4 s, 16 → 80.9-82.1 s (two runs), 32 → 82.2 s.
POOL_SIZE=16 is hard-coded in `crates/lib/src/write/parallel_writer.rs`. Over-contends
above 16.

---

#### skip_metadata in block_overlaps_diff (commit `b90e8ef`)

Single-line change: use `elements_skip_metadata()` in `block_overlaps_diff`
(only accesses element IDs, not metadata).

Germany hotpath (commit `1b10f18`, plantasjen):
- apply-changes-zlib: **6942ms → 5928ms (-15%)**

Larger improvement than expected - Germany's 18.4% rewrite fraction means
more blobs reach the precise `block_overlaps_diff` check (which decodes all
elements to test IDs against the diff). Skipping metadata decode saves ~1s
across ~11K precise-check invocations.

