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

The `--type` filtered path (full decode+re-encode) and the `--clean` path
both use `parallel_classify_phase` per kind with framed-output streamed via
`ReorderBuffer` (commits `6184602` + `b347c0a`, 2026-04-27). Planet
`cat --clean version`: **5m34s wall, 750 MB peak anon** (UUID `f2315551`
at `4fc8e35`, 2026-04-27 overnight; previously `7c4e03eb` 5m48s/835 MB at
`b347c0a` re-bench - 14 s wall, 10 % anon improvement attributable to
single-shot variance).

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
  worker pool); overlap candidates go to a long-lived worker pool.
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
re-compress on the writer thread, and at planet's ~85 MB of changed
output zlib's serial deflate becomes the bottleneck. zstd:1 narrowly
beats `none` (parallel-compressed bytes leave the writer faster than
uncompressed bytes through the same pipe). For OSC-only daily-diff
pipelines that don't need osmium-interop output, zstd:1 is the right
default.

## Add-locations-to-ways

Dense mmap index: 16B slots × 8 bytes = 128 GB virtual address space.
Only touched slots consume physical memory.

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

### Dense vs Sparse vs External index (plantasjen)

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

Dense is fastest when the working set fits in RAM. External uses ~1.6 GB
anon RSS at Europe scale via 4-stage radix join pipeline (node-only wire
scanner for stage 2, scatter buffer for stage 3, sequential reader for
stage 4).

Current Europe external baseline on `main` (post-A1 commit `0dc8ae1`):
**270.8 s** (`4m31s`, UUID `0b89f986`, `--bench 1`). A1 (rankless
node-ID bucketed stage 1+2, landed 2026-04-25) cut europe from
291.6 s at `6d71053` (-7.1 %) by replacing the two-pass way scan +
rank index with single-pass IdRecord emission and a streaming
merge-walk. See `notes/altw-external.md` for the full A1 chain.

**Crossover point**: between Japan (2.4 GB, dense 2x faster) and Europe
(33.6 GB, external 7.4x faster). At Europe scale, dense's mmap working set
(~16 GB) exceeds available RAM, causing thrashing. External's sequential
I/O stays bounded.

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

### Current planet baselines (commit `16e3694`, plantasjen)

The consolidated headline table. All rows `--bench 1` unless noted.

| Command | Mode | Wall | UUID | Notes |
|---|---|---:|---|---|
| cat (indexdata generation) | `--bench 1` | **86.5 s** | `5d90623f` | commit `aee7727` |
| cat --type way | `--bench 3` | 45.3 s | `2fe62148` | |
| cat --type relation | `--bench 1` | 47.7 s | `fba6e13e` | |
| cat --clean version | `--bench 1` | **333.8 s (5m34s)** | `f2315551` (4fc8e35, 2026-04-27 overnight) | `parallel_classify_phase` + reorder-buffer streaming, 750 MB peak anon |
| cat --dedupe | `--bench 1` | **7,981 s (133m)** | `1794f8a6` | single-threaded MERGEPBF path - see callout below |
| check --refs | `--bench 1` | **53.8 s** | `7d9f5dfd` | |
| check --ids (streaming, default) | `--bench 1` | **56.4 s** | `b1fc4d2e` (4fc8e35, 2026-04-27 overnight) | parallel_classify_phase port, 457 MB peak anon |
| check --ids --full | `--bench 1` | **63.2 s** | post-`01c67da` | |
| getid (include mode) | `--bench 1` | **6.8 s cold / 0.2 s warm** | `264d9dbf` / `12e74756` | dispatch landing `19d3a62`, 2026-07-11; cache-state dominated, see ADR-0006 gates below |
| getid --invert | `--bench 1` | 91.0 s | `40f5bd52` | |
| getparents | `--bench 1` | **19.0 s** | `a7c064eb` | dispatch landing `2306fd9`, 2026-07-11; was 23.5 s (`11bc44dc`) |
| inspect default (index-only) | `--bench 1` | **6.5 s** | `c146f2bb` | |
| inspect --nodes `-j 16` | `--bench 1` | **49.4 s** | post-`01c67da` | |
| inspect --tags `-j 16` | `--bench 1` | **168.3 s** | `9d741341` / post-`01c67da` | |
| inspect --tags --type node | `--bench 1` | 71.3 s | `047ac2f9` | |
| inspect --tags --type way | `--bench 1` | 82.9 s | `959bda7c` | |
| inspect --tags --type relation | `--bench 1` | 8.8 s | `8daf5f04` | |
| inspect --extended | `--bench 1` | **820.7 s (13m41s)** | `19db1512` | full decode + extended counters |
| sort (already-sorted input) | `--bench 1` | 132.3 s | `b9c10a41` | see Sort regression flag below |
| sort `--io-uring` | `--bench 1` | 126.8 s | `9ce80125` | see Sort regression flag below |
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
| add-locations-to-ways `--index-type external` | `--bench 1` | 636.6 s | `abe2ebf2` (856efc3, 2026-07-13) | was **546.0 s** `7fd04130` (2026-04-26); see ALTW drift flag + inject-prepass A/B below |
| apply-changes (daily diff, `--osc-seq 4920`) | `--bench 1` | 756.3 s | `8e940f71` | |
| renumber | `--bench 3` | 204.5 s | `abd74459` | see Renumber +10 s below |
| diff-snapshots text `-j 16` | `--bench 1` | **227.5 s** | `22a5eb55` | |
| diff-snapshots osc `-j 16` | `--bench 1` | **293.8 s** | `cdcaa4f1` | |
| build-geocode-index | `--bench 1` | **424.8 s** | `2b412af4` (2026-04-26) | |
| merge-changes (planet, `--osc-seq 4913`, 1-OSC) | `--bench 1` | 44.2 s | `941a5784` (2026-04-28) | |
| merge-changes (planet, `--osc-range 4914..4920`, 7-OSC) | `--bench 1` | **54.7 s** | `b6e964cc` (2026-04-28) | parallel-drain landed (was 267.2 s `bef0f1fa`) |
| merge-changes (planet, `--osc-range 4914..4920 --simplify`, 7-OSC) | `--bench 1` | **73.7 s** | `3e3ef119` (2026-04-28) | parallel parse + parallel write_simplified landed (was 262.2 s `c0d140b6`) |

> **Sort `+6-7 %` regression flag - softened by 4fc8e35 hotpath.**
> Both default and `--io-uring` sort on planet drifted slightly slower
> at `16e3694` vs the prior `1f97fae` / `68e1ba0` baselines from
> 2026-04-24. Inside single-shot bench noise on a sub-150-s wall, but
> the direction was consistent across both backends. A `sort --hotpath`
> at `4fc8e35` (UUID `d64932d2`, 2026-04-27 overnight) ran in **115.4 s**
> total - of which 108.6 s (94 %) was `pbfhogg::write::writer::flush`
> and 6.77 s (6 %) was `build_blob_index`. That hotpath wall sits
> *below* both the 124.6 s `68e1ba0` baseline and the 132.3 s `16e3694`
> regression on the same writer-flush-dominated phase mix. Hotpath has
> single-digit-percent instrumentation overhead on this command shape,
> so a fresh `--bench 1` is still wanted to settle cleanly, but the
> data point is consistent with "drift not real" rather than "drift
> confirmed". Possible drivers if the regression *were* real remain the
> truncation-handling commits (`436998b`, `12699db`).

> **ALTW drift flag + `--inject-prepass` A/B (2026-07-13, `856efc3`,
> plantasjen).** Same-commit single-sample pair: flag-OFF **636.6 s**
> (`abe2ebf2`), flag-ON `--inject-prepass` **602.9 s** (`b3b79a62`).
> The flag-ON run measured 33.7 s FASTER than flag-OFF despite doing
> strictly more work (2.63 B pinned refs, 535.3 M field-20 bitmaps,
> 145.8 MB of field-5 payload), so single-sample noise on this command
> is at least ~35 s / ~6 % - the injection cost is below `--bench 1`
> resolution, and ADR-0007's planet regression gate closes as "no
> measurable regression". Separately, BOTH runs sit +10-17 % above the
> 546.0 s April baseline (`7fd04130` at `16e3694`, 2026-04-26): larger
> than the observed noise band, so genuine drift across the ~2.5 months
> of commits is plausible but unconfirmed - a `--bench 3` pair (HEAD vs
> `--commit 16e3694`, grouped per the build-thrash rule) would settle
> it. Injection counters, first end-to-end exercise, all plausible:
> `altw_member_ways` 37.2 M, `altw_pinned_refs` 2.63 B (21 % of the
> 12.44 B way refs), `altw_field20_ways_emitted` 535.3 M (46 % of
> 1.166 B way messages), `altw_field5_bytes` 145.8 MB.

> **`cat --dedupe` planet 133-minute wall.** Single `MERGEPBF` phase,
> peak anon RSS only 1.4 GB, avg cores 1.3 - the path is essentially
> single-threaded for the full 87 GB input. `cat --type way`
> passthrough is 45.3 s on the same input, so the dedupe overhead is
> ~175× the passthrough wall. Workload is the BTreeMap-backed
> "newest-version-per-id" pass; not a regression but a clear `O(N)`
> parallelisation opportunity if `cat --dedupe` becomes a recurring
> planet workload.

> **Renumber `+10 s` (194 → 204.5 s).** The only headline row pointing
> the wrong way; both `--bench 3`, several dozen unrelated commits
> in-between, ~5 % is inside variance but not comfortably. Not a
> release blocker and not steady-state critical; shelved.

### Blob-count threshold dispatch landing gates (ADR-0006, plantasjen, 2026-07-11)

getparents and getid include mode dispatch between the pread header
walker and a full-file scan at an estimated 150,000 OSMData blobs
([`decisions/0006`](../decisions/0006-blob-count-threshold-dispatch.md)).
Landing commits `3adb44c` (getparents), `2306fd9` (getid), `dad28de`
(batch-parallel classify fix), `19d3a62` (getid streaming-arm fix).
Gate cells, all measured at HEAD of the day:

| cell | arm (estimate) | wall | UUID | reference (2026-07-10) | verdict |
|---|---|---:|---|---:|---|
| getparents planet primary | Walker (36,063) | **19.0 s** | `a7c064eb` | 23.5 s walker | pass |
| getparents europe `--bench` | FullScan (458,132) | **22.2 s** | `9f8602a2` | 26.4 s scan | pass |
| getparents planet 8k | FullScan (899,866) | 62.0 s | `595e8d7e` | 52.8 s scan (`2b3e496e`) | pass by same-day A/B: `68e1ba0` re-run today 69.4 s (`0e2c2313`), HEAD -11 % |
| getid planet primary | Walker (36,063) | 6.8 s cold / **0.2 s warm** | `264d9dbf` / `12e74756` | 6.1 s walker | pass; cache-state dominated |
| getid europe `--bench` | FullScan (458,132) | **17.6 s** | `6b9ad93c` | 17.9 s scan | pass |
| getid planet 8k | FullScan (899,866) | 48.6 s | `ddf6fed4` | 33.2 s scan (`c0d89d8f`) | pass by same-day A/B: `51c662e` re-run today 48.2 s (`80e726bf`), HEAD +0.8 % |

Disk read on the 8k FullScan cells is the whole file (~96 GB - the
kind filter and prescreen skip decompression, not bytes); the walker
cells read only headers plus matching bodies. The 8k profiles confirm
the mechanism: no schedule/walk phase on the FullScan arm.

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
phases. Atomic index dispatch (no `Arc<Mutex<Receiver>>`). Output
defaults to zlib:1. `mallopt(M_ARENA_MAX, 2)` inside
`renumber_external()` prevents glibc cross-thread arena fragmentation.

### Planet (87.7 GB indexed, 11.6B elements, plantasjen)

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways /
14,124,889 relations / 12,435,459,911 way refs.

### Memory

Peak anon 3.3 GB (commit `cb99106`). Single shared `IdSetDense` with
`AtomicU8::fetch_or` for concurrent pass 1 writes (~1.5 GB node bitset
+ rank index, ~200 MB way bitset + rank, ~20 MB relation bitset).
Zero temp disk. `mallopt(M_ARENA_MAX, 2)` inside `renumber_external()`
caps glibc arena growth from cross-thread OwnedBlock `Vec<u8>` frees.

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
element against N regions, write to N sync-mode PbfWriters. 3-phase
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

Pass 2b dominates at 124 s / 29 % of wall. Pass 2 interp resolve is the
only sequential-by-design phase left (30.6 s / 7 %); parallelising it
is an unclaimed follow-up worth ~25 s at planet if driven under 7 min
becomes a goal.

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

Shard-based block-pair merge (opt-in via `-j/--jobs N`, commit
`06628d8` 2026-04-20, `--bench 1`). Both text and `--format osc`
paths parallelise over the same ID-range shard plan.

| UUID | Args | Wall | Peak anon RSS | vs sequential |
|---|---|---:|---:|---:|
| `22a5eb55` | `-j 16` (text, temp-file shape) | **227.5 s (3m48s)** | 586 MB | **9.5×** |
| `cdcaa4f1` | `-j 16 --format osc` (commit `16e3694`, 2026-04-26) | **293.8 s (4m54s)** | n/a | **7.6×** |

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
