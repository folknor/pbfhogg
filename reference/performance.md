# Performance Data

Consolidated runtime measurements across datasets and commands.

> **TAINTED BENCHMARKS WARNING (2026-04-18).** Bench numbers measured at any
> commit in `4ce7e93..c0ae9a7` (Apr 9-17 2026) on the affected commands
> (`add-locations-to-ways`, `build-geocode-index`, `cat --type`/`--dedupe`,
> `check-ids`, `diff`, `extract` non-simple, `getid`, `inspect --nodes`/`--tags`,
> `sort`, `tags-filter` non-invert) carry an unaccounted O(N) all-blobs-scan
> cost from a `has_indexdata` / `check_sorted_and_indexed` regression that was
> in effect during that window. Affected entries are marked `[TAINTED]`
> below - re-measure before relying on them. See `find_tainted_runs.py`
> for the full row list.

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

## Cat passthrough (indexdata generation)

No `--type` filter. Decompresses each blob to scan IDs/tags, reframes BlobHeader
with indexdata+tagdata, preserves original compressed bytes. No re-compression.

| Dataset | Size | Time | Notes |
|---------|------|------|-------|
| Denmark | 461 MB | **2.8s** | commit `69a127f`, buffered |
| Europe | 32.4 GB | 112s | commit `69a127f`, `--direct-io`, `--type node,way,relation` (filtered path, +3.8% size) |
| Planet | 87 GB | **86.5s** | commit `aee7727`, buffered, UUID `5d90623f`, `--bench 1` |

The historical planet row (497s / 8m17s buffered at `69a127f`) measured a
combined cost: sequential decompress+reframe plus the `has_indexdata` /
`check_sorted_and_indexed` O(N) all-blobs-scan regression that was live
through `4ce7e93..c0ae9a7`. The seek-raw fix (`aa3147c`, buffer-preserving
`BlobReaderSource` header walk) and the short-circuit fix (`ca6711e`)
together drop the planet run from 497s to 86.5s - a 5.8× improvement at
the same output shape. Passthrough remains buffered-only; `--direct-io`
adds alignment overhead without the concurrent read/write pattern that
makes it faster for merge.

The `--type` filtered path (full decode+re-encode) **OOMs on planet** (87 GB) on
30 GB host at ~25% through. Pipelined writer's rayon pool lacks backpressure.
Works on europe (32.4 GB).

## Read throughput

Count all elements, best of 3 runs. Commit `d387301` (multi-dataset), plantasjen.

| Dataset | Size | sequential | parallel | pipelined | blobreader | mmap |
|---------|------|-----------|----------|-----------|------------|------|
| Malta | 9 MB | 49 ms | 9 ms | 24 ms | 50 ms | 52 ms |
| Denmark | 487 MB | 2.86s | 463 ms | 1.46s | 2.93s | 2.93s |
| Norway | 1.4 GB | 8.4s | 1.33s | 4.9s | 8.9s | 8.8s |
| Japan | 2.4 GB | 14.5s | 2.1s | 8.0s | 15.2s | 15.2s |
| Germany | 4.7 GB | 26.9s | 4.2s | 13.0s | 27.8s | 27.6s |

North America (18.8 GB, 2.58B elements, commit `a6ebbfe`):
parallel 22s, pipelined 57s, sequential 130s.

## Write throughput

Decode all elements then write through BlockBuilder + PbfWriter to `/dev/null`.
Commit `d387301` (multi-dataset), plantasjen.

| Dataset | Size | sync-none | sync-zlib:6 | sync-zstd:3 | pipelined-none | pipelined-zlib:6 | pipelined-zstd:3 |
|---------|------|-----------|-------------|-------------|----------------|------------------|------------------|
| Malta | 9 MB | 136 ms | 282 ms | 172 ms | 123 ms | 130 ms | 128 ms |
| Denmark | 487 MB | 8.1s | 16.8s | 10.0s | 7.3s | 7.4s | 7.3s |
| Norway | 1.4 GB | 21.3s | 44.0s | 25.7s | 18.9s | 19.2s | 18.9s |
| Japan | 2.4 GB | 38.5s | 79.2s | 47.0s | 34.8s | 35.0s | 34.4s |
| Germany | 4.7 GB | 81.3s | - | - | 71.7s | - | - |

With pipelined writes, all compression modes converge to the decode + wire-format
serialization floor. Sync zlib:6 is 2.3x slower; pipelined hides the cost.

North America (18.8 GB, 2.58B elements, commit `a6ebbfe`):
pipelined zlib 4m27s, pipelined none/zstd ~4m20s, sync zlib 14m34s.

## Merge (apply-changes)

Best results per dataset. Commit `a6ebbfe` (NA), `a65a198` (multi-region),
`e7bbfa2` (Denmark latest). Plantasjen.

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

### Cross-pipeline optimization: skip_metadata in block_overlaps_diff

Commit `b90e8ef`: use `elements_skip_metadata()` in `block_overlaps_diff`
(only accesses element IDs, not metadata). Single-line change.

Germany hotpath (commit `1b10f18`, plantasjen):
- apply-changes-zlib: **6942ms → 5928ms (-15%)**

Larger improvement than expected - Germany's 18.4% rewrite fraction means
more blobs reach the precise `block_overlaps_diff` check (which decodes all
elements to test IDs against the diff). Skipping metadata decode saves ~1s
across ~11K precise-check invocations.

### Descriptor-first streaming pipeline (P1 + P1.5 + parallel pwrite, 2026-04-21, commits `719f306` → `80b37df`)

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
- P1.5: workers call `frame_blob_pipelined` inline and attach framed
  `Vec<u8>` chunks to `DrainItem::Rewritten`; drain uses
  `write_raw_owned` per chunk (avoids the writer's
  rayon-spawn-per-block dispatch).

Planet LOW altw + OSC 4913, `--bench 1`, plantasjen (source on
Banan/nvme1n1; cross-disk scratch on Booty/nvme0n1p3 for the
separate-drives experiment):

Parallel pwrite is now the default writer backend for `apply-changes`
(buffered fallback removed from that path on 2026-04-21); the columns
below show the three backends as measured during the decision.

| Config | Buffered (removed) | `--io-uring` | Parallel pwrite (default, POOL_SIZE=16) |
|---|---:|---:|---:|
| Same-disk, `--compression none` | 135.5 s | 108.6 s | 116.0 s |
| Same-disk, `--compression zlib:6` | 143.7 s | 137.1 s | 140.8 s |
| Same-disk, `--compression zstd:1` | 121.2 s | 99.4 s | 104.5 s |
| Cross-disk, `--compression none` | 95.4 s | 93.0 s | 99.0 s |
| Cross-disk, `--compression zlib:6` | 134.9 s | 127.9 s | 117.4 s |
| Cross-disk, `--compression zstd:1` | 87.1 s | 82.8 s | **80.9 s** |

Pre-flip baseline (commit `52c2c4b`, UUID `e81a9316`): 144.4 s.
Best post-flip: **80.9 s** (-44 %) at zstd:1 + cross-disk + parallel
pwrite.

Writer-backend rule (the reasoning that landed parallel pwrite as the
default, with `--io-uring` kept as an opt-in override for IOPS-bound
topologies):

- **Same-disk**: `--io-uring` wins at every compression level. Same-disk
  is IOPS-bound (reads compete with writes on one NVMe); io_uring's
  queue-depth batching alleviates contention more than multiple
  per-syscall pwrites.
- **Cross-disk** + zstd:1 / zlib:6: parallel pwrite wins (80.9 s at
  zstd:1, 117.4 s at zlib:6). Disk has write bandwidth headroom;
  parallel pwrite saturates it faster than a single-thread writer can.
  Compressed-output rule: cross-disk favours parallel pwrite at every
  compressed level measured.
- **Cross-disk** + `--compression none`: `--io-uring` wins. The 119 GB
  output is close enough to NVMe peak that queue-depth tuning beats
  per-syscall parallelism.

Pool-size sweep at cross-disk zstd:1 (plantasjen, Samsung 990 PRO):
4 → 89.2 s, 8 → 83.4 s, 16 → **80.9-82.1 s** (two runs), 32 → 82.2 s.
POOL_SIZE=16 is hard-coded in
[`src/write/parallel_writer.rs`](../src/write/parallel_writer.rs); the
comment explains the measurement. Over-contends above 16.

Memory + runtime shape at planet (buffered same-disk, `--compression
none`, commit `719f306`):

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

Setup notes:

- `RLIMIT_MEMLOCK` must be ≥16 MB for `--io-uring` to register its
  64×256 KB buffer pool. Raise with
  `sudo prlimit --pid=$$ --memlock=unlimited:unlimited` in the bench
  shell before running.
- Cross-disk benches override `[plantasjen].scratch` in `brokkr.toml`
  from the default `data/bench-tmp` to a path on the secondary NVMe.
  Reverted post-bench.
- The `[plantasjen.drives].target = "hdd"` label in the toml is
  separately misleading: brokkr writes bench output to `scratch`, not
  to `target`. The "hdd" classification refers to an unrelated cargo
  build dir.
- Full plan + synthesis + measurement log in
  [notes/apply-changes-opportunities.md](../notes/apply-changes-opportunities.md).

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

Current Europe external baseline on `main` (commit `e497e54`): **320.5s** [TAINTED]
(`5m21s`, UUID `4268196a`). Previous baseline `d3e13ed` was 333s; the
`e497e54` change replaces the `finalize_coord_payloads` consolidation
phase with an in-RAM `BlobLocationRouter` (see the stage-breakdown tables
below). Earlier step `d3e13ed` had dropped 400s → 333s by collapsing
three whole-file header-oriented scans into one shared metadata pass.

**Crossover point**: between Japan (2.4 GB, dense 2x faster) and Europe
(33.6 GB, external 7.4x faster). At Europe scale, dense's mmap working set
(~16 GB) exceeds available RAM, causing thrashing. External's sequential
I/O stays bounded.

### External join stage breakdown (Europe, commit `3d977a0`, plantasjen)

| Stage | Time | RSS (anon) | Description |
|-------|------|-----------|-------------|
| Stage 1 (way pass) | 79s | 3.6 GB peak | Pass A (per-way refcount sidecar) + pass B (rank records) + coord pass |
| Stage 2 (node join) | 90s | 7.1 GB peak | Parallel counting-sort per rank bucket, coord slice pread |
| Stage 3 (slot reorder) | 42.5s | 7.8 GB peak | Delta-varint encodes per-blob coord_payloads; no coord_slots written |
| Finalize | 26.5s | 7.9 GB peak | Sequential concat of worker temps + encoded straddlers |
| Stage 4 (assembly) | 129s | 3.1 GB peak | Per-blob pread of coord_payloads; de-interleave into PBF wire format |
| **Total** | **400s** | | |

### External join stage breakdown (Europe, commit `d3e13ed`, plantasjen)

Shared blob-metadata scan is now explicit and replaces the old repeated
header-only walks in stage 1 and stage 4.

| Phase | Time | RSS (anon) | Description |
|-------|------|-----------|-------------|
| Meta scan | 30.9s | 19 MB peak | Single reusable blob-metadata pass (`BlobMeta`) with tagdata state |
| Stage 1 (way pass) | 36.0s | 3.6 GB peak | Pass A + pass B; way schedule and node-blob mapping projected from metadata |
| Stage 2 (node join) | 92.9s | 7.15 GB peak | Parallel counting-sort per rank bucket, inline node-blob coord fill |
| Stage 3 (slot reorder) | 32.2s | 7.30 GB peak | Scatter 12-byte slot records directly into per-bucket `scatter_buf` |
| Finalize | 18.3s | 2.95 GB peak | Parallel `coord_payloads` tail |
| Relation scan | 14.3s | 0.85 GB peak | `collect_relation_member_node_ids()` between stage 3 and 4 |
| Stage 4 (assembly) | 90.6s | 3.25 GB peak | Way reframe + node decode/filter + relation passthrough |
| **Total** | **333s** | | |

### External join stage breakdown (Europe, commit `e497e54`, plantasjen) [TAINTED]

The finalize phase is replaced by an in-RAM routing table: worker tmp
files stay open and stage 4 preads directly from the right fd. No more
consolidated `coord_payloads` file. See `notes/altw-structural-reports.md`
item #8.

| Phase | Time | RSS (anon) | Description |
|-------|------|-----------|-------------|
| Meta scan | 28.5s | 16 MB peak | Single reusable blob-metadata pass |
| Stage 1 (way pass) | 36.9s | 3.66 GB peak | Pass A + pass B; unchanged from `d3e13ed` |
| Stage 2 (node join) | 91.0s | 7.57 GB peak | unchanged; overall RSS peak |
| Stage 3 (slot reorder) | 33.6s | 7.50 GB peak | unchanged; emits per-worker tmps (not finalized into a single file) |
| **Router build** | **0.163s** | 3.07 GB peak | Replaces finalize: walks manifests + straddler staging, encodes straddlers into RAM |
| Relation scan | 21.0s | 3.14 GB peak | `collect_relation_member_node_ids()` - single-sample variance vs baseline |
| Stage 4 (assembly) | 91.7s | 3.40 GB peak | `BlobLocationRouter::pread_blob_payload` routes to worker tmps or in-RAM straddlers |
| **Total** | **320.5s** [TAINTED] | | −12.5 s vs `d3e13ed` on single `--bench 1` sample (UUID `4268196a`) |

Direct phase saving is unambiguous: `18.3 s → 0.163 s` on finalize. Single-sample
wall delta is smaller than the phase saving because relation-scan and stage-4 wobble
±a few seconds between runs. Router stats: 56,692 way blobs → 56,437 worker /
255 straddler / 0 empty; 95 MB encoded straddler bytes held in RAM; 20.7 GB of
worker tmps kept open for stage-4 pread (no longer consolidated into a separate file).

The shared metadata pass replaced three separate scans on Europe:

- `s1_way_schedule_build_ms`: `24.8s -> 0.08s`
- `s1_node_map_build_ms`: `30.9s -> 0.12s`
- `s4_schedule_scan_ms`: `31.5s -> 0.14s`
- new `extjoin_meta_scan_ms`: `30.9s`

Net: about **87s** of repeated scan work collapsed to about **31s**, which is
why stage 1 dropped `91.4s -> 36.0s` and stage 4 dropped `122.7s -> 90.6s`.

### `seek_raw` BufReader-discard fix (2026-04-18, commit `aa3147c`)

`BlobReader::seek_raw(SeekFrom::Current(_))` was the hot-path "skip past
just-read blob body." Stdlib `Seek::seek` on `BufReader<File>` always
discards the buffer, so every blob-body skip forced a buffer refill
(~10× file-size read amplification at the default 256 KB buffer).
Fixed via a public `BlobReaderSource` trait: default `skip_relative`
falls through to `Seek::seek` (correct, slow), `BufReader<R>` impl
overrides to call `BufReader::seek_relative` (preserves buffer when
in-range). Internal hot-path methods (`next_header_skip_blob`,
`next_header_with_data_offset`) route through a new `skip_blob_body`
helper using the trait. Public `seek_raw(SeekFrom)` API unchanged for
absolute-seek case. Bound widening on `BlobReader::new_seekable` and
`IndexedReader::new` is the public-API impact (one-line workaround for
downstream library users with non-standard reader types).

Per-caller wall deltas (`--bench 1` single-shot, plantasjen, 2026-04-18):

| Caller / Command | Dataset | Pre-fix `ca6711e` | Post-fix `aa3147c` | Δ | Pre-UUID | Post-UUID |
|---|---|---:|---:|---:|---|---|
| extract --smart | Europe | 211.2 s | 195.2 s | **−16.0 s, −7.6 %** | `f7c2ccda` | `1bd5bbdf` |
| add-locations-to-ways --index-type external | Europe | 286.3 s | 270.7 s | **−15.6 s, −5.5 %** | `5233ed39` | `555de261` |
| add-locations-to-ways --index-type external | Planet | (skipped¹) | 700.6 s | within noise² | - | `e30f7ddc` |
| tags-filter | Europe | 91.7 s | 93.1 s | within noise (+1.5 %) | `2244b6e4` | `ea9d2440` |
| renumber | Planet | 218.6 s | 206.7 s | **−11.9 s, −5.4 %**³ | `ae91b114` | `878e7a99` |

¹ Planet ALTW pre-fix bench skipped to save the 12 min re-bench;
the inferred regression-fix-only baseline is the README's 698 s minus
~20 s of all-blobs-scan overhead ≈ 678 s.
² Planet META_SCAN is only 2.5 % of total wall, so even though the
phase shows the expected per-phase win at 17.5 s post-fix (vs ~30 s
pre-fix on this code path), the wall delta sits inside `--bench 1`
variance.
³ Renumber's −5.4 % is larger than the audit's 1-2 % prediction;
unclear whether real or noise without `--bench 3` repeat. Not a
regression in any case.

#### Planet refresh (commit `aee7727`, 2026-04-18, plantasjen)

Post-fix snapshot of every planet-capable command that was re-benched.
All rows carry the `ca6711e` short-circuit fix and the `aa3147c`
`BlobReaderSource` header-walk fix. Numbers are the current ground
truth for the README table; historical README rows (most from before
the regression window) are retained in the per-command sections below
for cross-reference.

| Command | Mode | Wall | UUID | README row before | Δ |
|---|---|---:|---|---:|---:|
| cat --type way | `--bench 3` | 45.3 s | `2fe62148` | 44 s | +1 s (noise) |
| getid | `--bench 1` | 43.8 s | `5a44889d` | 66 s | **−22 s** |
| getid --invert | `--bench 1` | 91.0 s | `40f5bd52` | 83 s | +8 s (noise) |
| check --refs | `--bench 1` | 70.2 s | `64e9a394` | 72 s | −2 s (noise) |
| tags-filter -R | `--bench 1` | 51.8 s | `f262f068` | 52 s | - |
| extract --smart (Europe bbox) | `--bench 1` | 267.5 s | `07dcdae3` | 282 s | **−14 s** |
| cat (indexdata generation) | `--bench 1` | 86.5 s | `5d90623f` | 497 s | **−410 s (5.8×)** |
| add-locations-to-ways --index-type external | `--bench 3` | 661.2 s | `a406d77e` | 698 s | **−37 s** |
| renumber | `--bench 3` | 204.5 s | `abd74459` | 194 s | +10 s (see note) |

The headline is the unfiltered `cat` row: the 497 s → 86.5 s drop is
the user-visible shape of the two header-walk fixes compounding on a
command that does nothing but walk headers + rewrite BlobHeader. The
old 497 s measurement was taken at `69a127f` (pre-regression), so this
isn't only a regression rollback - the post-fix path is substantially
faster than the pre-regression path too, because `seek_raw` was
shedding buffer on every blob long before the `has_indexdata` bug
shipped.

`getid` fell 22 s at the same time, which is in the right ballpark for
the `check_sorted_and_indexed` short-circuit saving ~20 s on
`has_indexdata`-gated subcommands - a second independent confirmation
of the short-circuit fix at planet scale.

Renumber's +10 s (194 s → 204.5 s) is the only row pointing the wrong
way. Both are `--bench 3`; the older 194 s was at `cb99106`, this is
at `aee7727`, so several dozen commits of unrelated churn sit
in-between. 5 % is inside `--bench 3` variance but not comfortably
inside - a deliberate bisect would tell us whether something landed
that genuinely costs. Not a release blocker and not in the critical
planet-pipeline path (once-per-schema renumber, not a steady-state
command), so shelving for now.

Three planet rows in the README table (`check --ids --full`,
`apply-changes`, `build-geocode-index`) were not re-benched in the
first round. All three have since been resolved:

- `check --ids --full`: re-benched at `ef6ce09` (`c498fff0`,
  `--bench 1`), **69.5 s / 1m10s**, down from the old tainted 1m33s
  row. Untainted - carries both post-fix short-circuit + seek-raw
  patches. See the `check --ids --full` section below for the
  post-fix row.
- `apply-changes` (daily diff, `--osc-seq 4920`): re-benched at
  `ef6ce09` (`8e940f71`, `--bench 1`), **756.3 s / 12m36s**, inside
  noise of the prior 753 s buffered+zlib planet row. No drift.
- `build-geocode-index`: cleared separately via the full optimisation
  arc (commit `82db8ed`, UUID `b4b25c05`, **432.9 s / 7m12s**). Not
  part of the refresh round - the arc landed independently.

#### Planet refresh (commit `06628d8`, 2026-04-20, plantasjen)

Two commands picked up large wins in the 2026-04-20 batch via a shared
`pread`-only header walker (`src/read/header_walker.rs::HeaderWalker`)
and shard-based parallel merge-join (`-j/--jobs N` on `pbfhogg diff`).

| Command | Mode | Wall | UUID | Row before | Δ |
|---|---|---:|---|---:|---:|
| getid (include mode) | `--bench 1` | **6.1 s** | `24362e36` (2026-04-20, 1-pread walker) | 43.8 s (2026-04-18) | **−37.7 s (7.2×)** |
| inspect (default metadata, index-only) | `--bench 1 --force` | **6.5 s** | forced, dirty tree | 21.4 s (`c146f2bb`, partially-cached pre-migration) | **−14.9 s (3.3×)** |
| inspect `--nodes -j 16` | `--bench 1` | **56.8 s** | `c5edebe7` | sequential (never stored; germany extrapolation ~370 s) | **~5-6× (new parallel path)** |
| inspect `--tags -j 16` | `--bench 1` | **169.5 s (2m50s)** | `9d741341` | sequential (never stored; germany extrapolation ~800 s) | **~4-5× (new parallel path)** |
| diff-snapshots text | `--bench 1 -j 16` | **227.5 s (3m48s)** | `22a5eb55` (2026-04-20, temp-file shape) | 2150.9 s (35m51s, sequential) | **−1923 s (9.5×)** |
| diff-snapshots --format osc | `--bench 1 -j 16` | **313.8 s (5m13s)** | `9b3fc2b9` | 2225.6 s (37m06s, sequential) | **−1912 s (7.1×)** |

All four carry the 2026-04-18 `ca6711e` + `aa3147c` short-circuit +
seek-raw fixes plus the 2026-04-20 patches:

- `HeaderWalker` (shared): opens the fd with `posix_fadvise(POSIX_FADV_RANDOM)`
  and walks blob headers via raw `pread`. Initial implementation did
  two preads per blob (length prefix + header); the 2026-04-20
  `d263d76` refinement collapses those to one 4 KB probe pread that
  covers the length prefix and the full header in the common case
  (real headers run ~100-200 B), with a tail-fallback pread only for
  the rare oversized header. Avoids the `BufReader` amplification
  where data bytes that happen to sit inside the 256 KB buffer window
  get read and discarded. Planet getid walker: 88 GB → 601 MB of disk
  read. Planet inspect default path: 36.3 GB → 14.2 GB disk read (the
  remaining 14 GB is kernel page-granularity fetches plus some
  residual readahead despite `fadvise(RANDOM)`).
- Shard-based parallel block-pair merge for `diff` text and osc. N-1
  thresholds at old-blob boundaries; straddling new blobs are read
  by both adjacent shards and each shard's element merge clips to its
  own `(t_low, t_high]` window. Both paths stream shard output to
  per-shard scratch temp files and concatenate in shard order; peak
  anon RSS ~586 MB (text) / ~663 MB (osc) at planet. Shard balance
  within 1.03× max/min on germany.

`getid` is walker-syscall-bound; the 2026-04-20 1-pread refinement
trimmed planet include-mode from 7.0 s to 6.1 s (-13 %, UUID
`24362e36`). The syscall floor remains the dominant cost. `diff`
osc's lower speedup is the serial `assemble_osc` gzip + concat of
~45 GB of XML fragment temp files (32.8 s / 10 % of wall).

`inspect --nodes -j 16` hits `avg_cores 14.7 / 16` (92 %) on the
decode + accumulate phase with peak anon 410 MB - per-worker state
is just scalar stats + a 128-entry coord block buffer, trivially
mergeable. `inspect --tags -j 16` hits `avg_cores 6.8 / 16` (42 %)
because the main-thread global-map merge is the serialising step;
peak anon 17.5 GB is the planet distinct-tag map plus glibc
anon-page retention from the per-blob hashmap churn. A prior
`parallel_classify_accumulate`-based shape pushed peak anon to
26.8 GB (each of 16 workers held a full-cardinality accumulator by
the end of the scan); the final `parallel_classify_phase` shape
emits per-blob maps to a single main-thread merger and avoids that
multiplier.

Previous-plan docs (`notes/diff-snapshots-opportunities.md`,
`notes/getid-include-optimization.md`) are retired.

#### Planet scan-audit refresh (commits `de8daf1`...`01c67da`, 2026-04-20, plantasjen)

The earlier 06628d8 refresh landed `HeaderWalker` on `getid`, `inspect`
default, and `diff` shards. The scan-audit sweep that follows pushes it
through every other in-tree header walk: the shared
`scan::classify::build_classify_schedule{,_split}` (Tier S - feeds
check-refs, check-ids, tags-count, node_stats, getid, tags-filter
expression path, extract Way fallback, geocode Pass 2 node schedule)
plus eight per-command schedule builders that didn't route through the
shared primitive (extract common + smart, multi-extract, renumber,
tags-filter single-pass + pass-2, geocode `build_pass2_schedules`,
ALTW external `scan_blob_metadata`, apply-changes `scan_node_blob_schedule`).
Two pattern-2 migrations (sequential `BlobReader` → parallel) also
landed for the dense ALTW path: `collect_way_referenced_node_ids`
(`parallel_classify_phase`, trades the `scan_way_refs` wire-format
fast path for 16× parallelism) and `collect_relation_member_node_ids`
(`parallel_classify_accumulate`, per-worker IdSet bounded ~68 MB).

Measured impact at the phase level, europe `--bench 3` with
`--stop <PHASE>`:

| Command | Phase | Pre (`1245cde`) | Post (`8e3a0d1`) | Δ |
|---|---|---:|---:|---:|
| check-refs | SCHEDULE_SCAN_LOOP | 24,684 ms | 432 ms | **57×** |
| tags-filter | TAGSFILTER_SINGLE_PASS_SCHEDULE_SCAN | 25,050 ms | 994 ms | **25×** |
| extract --simple | EXTRACT_SCHEDULE_SCAN | 24,584 ms | 471 ms | **52×** |

Planet full-command walls (single-sample `--bench 1`, ±5 % run-to-run
noise from cache state alone):

| Command | Pre (`1245cde`) | Post (`8e3a0d1` / `01c67da`) | Δ |
|---|---:|---:|---:|
| tags-filter (transitive) | 147.5 s | **119.9 s** | -18.7 % |
| inspect --nodes -j 16 | 58.1 s | **49.4 s** | -15.0 % |
| check-refs | 72.6 s | **62.7 s** | -13.7 % |
| check-ids --full | 72.5 s | **63.2 s** | -12.8 % |
| extract --simple | 264.7 s | **247.3 s** | -6.6 % |
| multi-extract (5 regions) | 1004.6 s | **972.0 s** | -3.2 % |
| extract --complete | 261.8 s | **254.2 s** | -2.9 % |
| add-locations-to-ways (external) | 684.0 s | **673.6 s** | -1.5 % |
| build-geocode-index | 430.5 s | **434.9 s** | +1.0 % (noise) |
| renumber | 215.6 s | **219.3 s** | +1.7 % (noise) |
| extract --smart | 278.7 s | **283.5 s** | +1.7 % (noise) |
| apply-changes --osc-seq 4920 | 577.0 s | **589.1 s** | +2.1 % (noise) |
| inspect --tags -j 16 | 169.5 s | **168.3 s** | -0.7 % |

Full-command wins shrink or disappear at europe scale because the old
buffered header walk was accidentally prefetching blob bodies via the
kernel's sequential readahead; the downstream decompression pass
reused those warm pages. `HeaderWalker`'s `posix_fadvise(POSIX_FADV_RANDOM)`
deliberately skips that prefetch. At planet the file is ~4× physical
RAM so the prefetched pages would be evicted before decompression
could reuse them anyway, and the header-walk savings land cleanly on
the bottom line. At europe the two effects cancel.

Three now-dead methods removed post-migration:
`BlobHeader::{index, tag_index}` and
`BlobReader::next_header_with_data_offset`.

`notes/scan-optimization-audit.md` retires the high-leverage Tier 1
items; dense node index Pattern 2 (`build_node_index_dense`), O(1)
probes (`check_sorted_and_indexed`, `has_indexdata`), and unsorted
extract paths are intentional non-goals per the audit doc itself.

#### Europe ALTW phase breakdown (the cleanest signal)

`EXTJOIN_META_SCAN` is the only ALTW phase that walks blob headers; all
other stages use direct `pread` on the file (no `BlobReader`, no
`seek_raw`).

| Phase | Time (post-fix) | Time (pre-fix `ca6711e`) | Δ |
|-------|----------------:|-------------------------:|---:|
| Meta scan | 13.3s | 25.9s | **−12.6 s, −49 %** |
| Stage 1 (way pass) | 35.3s | 37.1s | −1.8 s |
| Stage 2 (node join) | 90.9s | 94.3s | −3.4 s |
| Stage 3 (slot reorder) | 32.9s | 33.2s | −0.3 s |
| Router build | 0.17s | 0.17s | 0 |
| Relation scan | 3.9s | 3.9s | 0 |
| Stage 4 (assembly) | 93.0s | 90.5s | +2.5 s (single-shot variance) |
| **Total** | **270.7 s** | **286.3 s** | **−15.6 s, −5.5 %** |

UUIDs: post-fix `555de261`, pre-fix `5233ed39` (both `--bench 1`).
Meta scan delta is the direct effect of `BufReader::seek_relative`
preserving the buffer on every blob-body skip; the small downstream
deltas in stages 1+2 are page-cache benefit (header walk used to amplify
reads ~10× and push subsequent stages' working set out of cache).

### External join planet (commit `aa3147c`, post-`seek_raw` fix)

Planet wall **700.6 s** (UUID `e30f7ddc`, `--bench 1`), basically
identical to the pre-regression README baseline of 698 s (within
single-shot variance). META_SCAN measures 17.5 s post-fix - vs the
inferred ~30 s pre-fix on the same code path, so the seek_raw fix
saves ~12 s in that phase. As a fraction of planet total wall (700 s),
that's 1.7 %, comfortably inside the noise floor of `--bench 1`.

The audit's 10-15 % wall prediction was based on Europe ratios where
META_SCAN is 9 % of total (28.5 / 320 s); planet's META_SCAN is only
2.5 % of total because Stages 1+2+4 dominate (~85 % combined). The
fix delivered exactly what it should at the phase level - wall just
doesn't move much because the targeted phase is small.

Phase breakdown (UUID `e30f7ddc`):

| Phase | Time |
|---|---:|
| META_SCAN | 17.5 s |
| STAGE1 (PASS_A + PASS_B) | 123.2 s |
| STAGE2 | 240.5 s |
| STAGE3 | 85.5 s |
| ROUTER_BUILD | 1.6 s |
| RELATION_SCAN | 6.3 s |
| STAGE4 | 223.3 s |
| **Total** | **700.6 s** |

### External join optimization history

| Version | Denmark | Europe | Planet | Commit |
|---------|---------|--------|--------|--------|
| Original (256x re-read) | 302s | - | - | `034422c` |
| Single-pass merge | 25s | 2,060s | - | `a334c72` |
| + fadvise + mmap coord_slots | 22s | 1,824s | - | `165cbb2` |
| Node-only scanner + scatter buffer | 14s | 921s | - | `ee9b19f` |
| + blob skip + pool reuse | 14s | ~901s | - | `d272b49` |
| P2b/P2c parallel assembly | - | 608s | - | `6b09796` |
| External radix permutation (full) | 14s | 422s | 1,462s | `b0a5fb8` |
| Stage 1B overlap + misc | - | 392s | 1,075s | `091fc5b` |
| **coord_payloads integrated** | **7.4s** | **400s** | **953s** | **`3d977a0`** |
| **+ shared blob metadata scan** | - | **333s** | - | **`d3e13ed`** |
| **+ BlobLocationRouter (no finalize consolidation)** | - | **320.5s** | - | **`e497e54`** |

The coord_payloads integration (2026-04-14) was pursued primarily for
non-wall benefits. Planet measured −29 s wall as a pleasant surprise;
Europe +7 s. The structural wins are: scratch peak 300 GB → 256 GB
(−44 GB), stage-4 major faults 555K → 3.2K (−99.4%), stage-4
delta-encode CPU 68 s cumulative → 0, no more 99 GB `coord_slots`
mmap thrashing across 6 workers.

Key earlier optimizations: node-only wire scanner (bypasses
PrimitiveBlock, eliminates 25 GB heap retention), scatter buffer
(eliminates sort + 4.69B pwrite calls, 15x speedup), BlobReader
fadvise(DONTNEED) (general infrastructure), deferred IdSetDense,
buffer reuse in bucket loads, shared blob metadata scan (collapses
three repeated header passes into one reusable `BlobMeta` vector).

See [altw-optimization-history.md](../notes/altw-optimization-history.md)
for the full investigation and memory optimization research log.

## Check commands (post-optimization)

Optimization history for `check --refs` and `check --ids --full`. Both
followed the same two-step arc that dropped planet-equivalent workloads
from tens of minutes to under two minutes: swap per-type `RoaringTreemap`
for `IdSetDense`, then three-phase parallelize via `parallel_classify_phase`.
The `roaring` crate dependency was dropped entirely from pbfhogg after
these landed.

### check --refs (commits `8f0ccbb`, `053def6`, `fbf591c`, 2026-04-17, plantasjen)

Sequential main-thread consumer pegged at 100 % CPU on `RoaringTreemap::insert`
pre-swap. Step #1 swapped to `IdSetDense` (O(1) insert/contains, purpose-built
for dense-monotonic OSM IDs). Step #2 parallelized via `build_classify_schedules_split`
(one header walk, per-kind schedules) + three `parallel_classify_phase` phases.

| Dataset | Pre-swap | Post-swap (seq) | Post-parallel | Cumulative |
|---------|---------:|----------------:|--------------:|-----------:|
| Japan   | 56.7 s `09484939` | 33.1 s `1fd77d78` | **2.1 s** `4a347e3b` | 27× |
| Europe  | 426.2 s `fb042f27` | - | **33.6 s** `70ff6c5d` | 12.7× |
| Planet  | 1225 s (`7e9c2e9` baseline) | - | **72.5 s** `862547e4` | **16.9×** |

Europe phase breakdown post-parallel (UUID `70ff6c5d`):

| Phase | Wall |
|---|---:|
| SCHEDULE_SCAN_LOOP (one header walk, all kinds) | 14.8 s |
| CHECKREFS_NODES (parallel scan) | 11.2 s |
| CHECKREFS_WAYS (parallel scan) | 7.4 s |
| **Total** | **33.6 s** |

Planet phase breakdown post-parallel (UUID `862547e4`):

| Phase | Wall |
|---|---:|
| SCHEDULE_SCAN_LOOP (one header walk, 92 GB file) | 16.8 s |
| CHECKREFS_NODES (parallel scan) | 35.4 s |
| CHECKREFS_WAYS (parallel scan) | 20.2 s |
| CHECKREFS_DEFERRED_RESOLVE | 0 ms |
| **Total** | **72.4 s** |

Planet memory: peak 2.17 GB, p95 2.13 GB, p50 1.13 GB, 0 major page faults.
Pre-allocated `IdSetDense` for 14 B node IDs (1.6 GB resident for the
duration of phases 1+2) is the dominant contributor.

Plan target was 6-10 min at planet (post-step-#2 projection). Measured
1 min 12 s, ~5-8× under the plan floor. Step #3 (selective wire-format
parser) was not needed at these numbers.

### check --ids --full (commits `855b3b2`, `0d71b3b`, 2026-04-17, plantasjen) [TAINTED]

Only remaining `roaring` consumer before the swap. Streaming mode (default
`check --ids`) was constant-memory and unchanged; `--full` mode allocated
per-type `RoaringTreemap`s for duplicate detection. Swap mirrors
check-refs step #1 (adds `IdSetDense::set_if_new` / `set_atomic_if_new`
methods for the "is this ID new?" signal that `RoaringTreemap::insert`
previously provided). Parallel rewrite mirrors check-refs step #2; uses
the widened `parallel_classify_phase` merge signature (`FnMut(usize, R)`)
for seq-ordered cross-blob monotonicity checks.

| Dataset | Pre-swap | Post-swap (seq) | Post-parallel | Cumulative |
|---------|---------:|----------------:|--------------:|-----------:|
| Europe  | 312.6 s `6ca113a8` [TAINTED] | 172.0 s `32d8a631` [TAINTED] | **52.7 s** `31ca231d` [TAINTED] | 5.9× |
| Planet  | - | - | **93.2 s** `2f52252d` [TAINTED] | n/a (pre-swap not benched) |

Post-fix planet re-bench (commit `ef6ce09`, 2026-04-18, UUID
`c498fff0`, `--bench 1`): **69.5 s / 1m10s**, carrying both the
`ca6711e` short-circuit fix and the `aa3147c` `BlobReaderSource`
seek-raw fix. Untainted replacement for the 93.2 s row; the 23.7 s
drop is consistent with the ~20 s short-circuit saving observed on
other `has_indexdata`-gated subcommands (`getid`, `cat`).

Planet phase breakdown (UUID `2f52252d`) [TAINTED]:

| Phase | Wall |
|---|---:|
| pre-schedule setup (`ElementReader::open` + 3× `IdSetDense::pre_allocate` ~1.86 GB) | 22.2 s |
| SCHEDULE_SCAN_LOOP (one-pass header walk, 92 GB file) | 16.8 s |
| VERIFYIDS_NODES parallel scan | 36.6 s |
| VERIFYIDS_WAYS parallel scan | 17.0 s |
| VERIFYIDS_RELATIONS parallel scan | 0.5 s |
| **Total** | **93.1 s** |

Planet memory (UUID `2f52252d`, 932 /proc samples) [TAINTED]:

| Metric | Value |
|---|---:|
| Peak RSS | 2.22 GB |
| p95 RSS | 2.15 GB |
| p50 RSS | 644 MB |
| Major page faults | 0 (never touched swap) |
| Host available | ~27 GB |

`IdSetDense::pre_allocate` is ID-space bounded (14 B nodes + 1.5 B ways +
25 M relations ≈ 1.86 GB), not population-bounded, so planet peak RSS is
~the same as Europe.

## CLI commands

Commit `aacbe80`, plantasjen. Best of 3 runs.

### Denmark (487 MB indexed, 59M elements, commit `6fc1283`, plantasjen)

| Command | Time |
|---------|------|
| tags-filter-osc | 14 ms |
| inspect (indexdata) | 19 ms |
| cat --type relation | 85 ms |
| tags-filter highway=primary | 152 ms |
| inspect-tags --type way | 251 ms |
| sort (sorted, indexdata) | 366 ms |
| getid | 379 ms |
| getparents | 400 ms |
| tags-filter amenity=* | 438 ms |
| apply-changes | 517 ms |
| cat --type way | 239 ms |
| merge-changes | 107 ms |
| inspect-tags | 1.61s |
| diff --format osc | 1.6s (`1a42c27`) [TAINTED] |
| inspect-nodes | 1.73s |
| check --ids | 1.87s |
| getid --invert | 0.5s |
| extract --simple | 2.48s |
| extract --complete | 2.40s |
| tags-filter two-pass | 2.62s |
| extract --smart | 2.65s |
| add-locations-to-ways (external) | 7.4s |
| check --refs | 6.83s |
| time-filter | 9.39s |
| cat --dedupe | 22.4s |
| renumber | 22.3s |

### Japan (2.4 GB indexed, 344M elements, plantasjen)

Baseline commit `aacbe80`. Entries marked with † were improved by later
optimizations and show the latest measured value.

| Command | Time | Notes |
|---------|------|-------|
| inspect (indexdata) | 92 ms | |
| tags-filter-osc | 169 ms | |
| cat --type relation | 306 ms | |
| cat --type way | 0.7s | † raw passthrough, `c33e8cc` |
| tags-filter highway=primary | 840 ms | |
| sort (sorted, indexdata) | 1.33s | |
| getid --invert | 1.3s | † raw passthrough, `c33e8cc` |
| merge-changes | 1.62s | |
| getid | 1.94s | |
| getparents | 2.06s | |
| tags-filter amenity=* | 2.20s | |
| inspect-tags --type way | 2.43s | |
| apply-changes | 2.53s | |
| extract --complete | 4.4s | † parallel classify |
| inspect-tags | 4.82s | |
| extract --smart | 5.2s | † parallel classify |
| inspect-nodes | 9.14s | |
| extract --simple | 9.36s | |
| check --ids | 10.4s | |
| tags-filter two-pass | 13.7s | |
| check --refs | 38.7s | |
| time-filter | 43.8s | |
| add-locations-to-ways | 64.1s | |
| diff | 72.2s | |
| diff --format osc | 73.1s | |
| cat --dedupe | 102.2s | |
| renumber | 152.4s | |

### Germany (4.7 GB indexed, ~496M elements)

Hotpath profiling, commit `1b10bfd`, plantasjen.

| Test | Time | RSS | Notes |
|------|------|-----|-------|
| inspect-tags | 23.9s | 1.6 GB | decompress_blob 28.7s cumulative (parallel), pipeline 12.1s |
| check-refs | 74.1s | 4.6 GB | 99.97% in pipeline, single-threaded consumer bound |
| cat --type (zlib) | 61.8s | 10.9 GB | frame_blob 193s cumulative (parallel zlib), add_node 22.6s (429M), add_way 22.8s (70M) |
| apply-changes zlib | 6.2s | 395 MB | classify 2.9s, rewrite+output 2.1s |
| apply-changes none | 4.4s | 252 MB | classify 1.2s, rewrite+output 1.9s |

Allocation profiling (same commit):

| Test | Net Alloc | Cumulative | Key finding |
|------|-----------|------------|-------------|
| inspect-tags | 3.0 GB | 25.7 GB | decompress_blob 5.1 GB, wire::parse 3.1 GB |
| check-refs | 2.4 GB | 4.0 GB | wire::parse 3.0 GB (126%), nearly all in block::new |
| cat --type (zlib) | 175 MB | 240 GB | take_owned 41 GB, add_way 14.8 GB, decompress 6.9 GB |
| merge zlib | 293 MB | 29.6 GB | rewrite_block_parallel 17.3 GB, read_raw_frame 4.4 GB |
| merge none | 293 MB | 31.7 GB | same pattern, RSS under 300 MB |

Previous commit data (commit `46f7388`):

| Command | Time |
|---------|------|
| add-locations-to-ways | 64.5s |

### vs osmium (Denmark, commit `23862d1`)

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| sort (sorted, indexdata) | **0.14s** | 11.6s | **83x** |
| apply-changes (indexdata + zlib) | **2.7s** | 7.2s | **2.7x** |
| tags-filter w/highway=primary -R | **0.24s** | 0.56s | **2.3x** |
| cat --type way (indexdata, raw passthrough) | **0.24s** | 2.22s | **9.3x** |
| add-locations-to-ways | **8.3s** | 12.6s | **1.5x** |
| check --refs | **4.8s** | 4.5s | 0.94x |

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

Commit `6165394`, dirty `--force --bench 1`. Single-sample.

| Phase | Duration | Peak Anon | Share |
|---|---:|---:|---:|
| PASS1 nodes | **169 s** | 610 MB | 15.5% |
| STAGE2A way emit | **119 s** | 934 MB | 10.9% |
| STAGE2B node merge-join | **381 s** | **7.32 GB** | 34.9% |
| STAGE2C slot reorder | **146 s** | 3.03 GB | 13.4% |
| STAGE2D way assembly | **129 s** | 809 MB | 11.8% |
| R1+R2A fused | 29 s | 1.04 GB | 2.7% |
| R2B rel merge-join | 68 s | 2.03 GB | 6.2% |
| R2C + R2D | 34 s | 1.04 GB | 3.1% |
| **TOTAL** | **1,092 s (18.2 min)** | **7.32 GB** | - |

Stage 2b breakdown (cumulative across 2 workers):
  load_way_refs 276 s, radix_sort 243 s, load_node_map 101 s,
  merge_join 130 s. Stage 2b is the #1 remaining target.

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways / 14,124,889
relations / 12,435,459,911 way refs. All match the first-measurement
baseline (`c5d00c22`) exactly.

### Optimization history

| Commit | Change | Planet Time |
|--------|--------|-------------|
| `e156e97` | First planet measurement (sequential all stages) | **3,456 s (57.6 min)** |
| `cc80442` | Stage 2b LSD radix sort | - (Denmark only) |
| `a478ae8` | Halve map-bucket format (drop new_id field) | - |
| `37ff902` | Stage 2b 2-worker bucket parallelism | - |
| `8ec298c` | Pass 1 parallel decode (worker pool) | - |
| `34a6b7c` | Stage 2d parallel decode (worker pool) | - |
| `e7219f0` | Stage 2a parallel scan (worker pool) | - (OOM on planet, see below) |
| `9695ad5` | Writer backpressure (permit pool) | - (still OOM) |
| `f607842` | Work-stealing dispatch for pass 1 + stage 2d | **2,033 s (33.9 min)** |
| `d3da65f` | Two-cursor merge + PrimitiveBlock copy fix | **1,901 s (31.7 min)** |
| `dc13a7b` | DenseNodes wire-format rewriter + 4 workers + mallopt | **1,468 s (24.5 min)** |
| `48183b5` | Way wire-format rewriter for stage 2d | **1,334 s (22.2 min)** |
| `dc13a7b` | DenseNodes rewriter + 4 workers + mallopt | **1,468 s (24.5 min)** |
| `d11166b` | Stage 2d 4 workers | **1,325 s (22.1 min)** |
| `6165394` | 14-opt batch: splice, parallel 2c, schedule reuse, batch writes | **1,092 s (18.2 min)** |
| `7839303` | Stage 2b/2c 4 workers + radix 4 passes | **960 s (16.0 min)** |
| `9ec5eda` | IdSetDense rank fusion (eliminates stage 2a+2b+2c) | **505 s (8.4 min)** |
| `c5c0e08` | Build way_id_set during stage 2d | **479 s (8.0 min)** |
| `ae45fd6` | Eliminate way_map files + mmap R2B scatter | **442 s (7.4 min)** |
| `94bf351` | Pass 1 back to 4 workers, fuse R1+R2A+R2B | **442 s (7.4 min)** |
| `cbffb45` | Wire-format splice rewriter for R2d relations | **412 s (6.9 min)** |
| `71bb548` | Parallel R2d (work-stealing + member-count sidecar) | **401 s (6.7 min)** |
| `dd3f477` | zlib:1 output + IdSetDense::resolve() combined lookup | - |
| `1b171f0` | Inline IdSetDense::set() during reframe, eliminate old_ids_out | - |
| `fefd357` | Cache blob schedules across all phases | **360 s (6.0 min)** |
| `b71bae9` | Fuse relation resolve into R2d, eliminate all temp files, zero scratch disk | - |
| `feb3099` | Denser rank() blocks (64B instead of 256B) + respect compression flag | - |
| `6acb9eb` | Replace relation_map FxHashMap with IdSetDense (~500 MB → ~20 MB) | - |
| `db49c92` | Open input file once, reuse fd across all phases | - |
| `67c7960` | Atomic index dispatch + reframe_buf pre-reserve | **209 s (3m29s)** |
| `cb99106` | Shared atomic IdSetDense (−54% memory), wire-format R1 scanner | **194 s (3m14s)** |

**−3,262 s (−94%)** from baseline. Each commit verified on Denmark
(`brokkr verify renumber`, 306-relation orphan delta preserved exactly).
Two intermediate planet runs OOM-killed at ~26 GB anon RSS due to
reorder-buffer backlog from range-split dispatch and glibc arena
fragmentation - resolved by work-stealing dispatch + `MALLOC_ARENA_MAX=2`.

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
scanning the input file - the prior Europe×2.6 = 26-28 GB projection
was wrong by ~2.4×. The mechanism identified during the 2026-04-10/11
investigation was a cold-arena-page residency cascade triggered by
post-PASS1 header scans touching pages glibc had reserved but not
populated; fixed by plumbing the PASS1 schedule forward into PASS2
and PASS3 via `Pass1Result::pass3_blob_schedule` and
`pread_write_pass_with_schedule`.

Denmark bbox `12.4,55.6,12.7,55.8`, Japan bbox `139.5,35.5,140.0,36.0`,
Europe and Planet bbox `-25.0,34.0,45.0,72.0` (full-continent).

Simple extract uses a 3-phase barrier pipeline with parallel classification
and raw frame passthrough. Each phase (nodes, ways, relations) classifies
blobs in parallel then writes matching raw frames via pread workers - no
decode+re-encode. Japan simple: 3.8s vs osmium 7.2s (1.9x faster). Europe
simple: 96.3s (was 350s sequential, was OOM with pipelined reader).

Complete-ways and smart pass 1 (`collect_pass1_generic`) uses three-phase
parallel pread classification (nodes → ways → relations) via a reusable
`parallel_classify_phase` helper. Smart pass 2 (way dependency resolution)
also uses `parallel_classify_phase`, replacing the old sequential BlobReader
scan. Workers pread + decompress + classify in parallel, sending compact
results back to the consumer. Japan complete: 19.7s → 3.7s (5.3x), smart:
24.3s → 4.7s (5.2x). Both beat osmium (complete 2.5x faster, smart 2.6x
faster at earlier measurements). Write passes use pread-from-workers with
full PrimitiveBlock lifecycle per worker.

**PASS1 schedule reuse (commits `d4ea760`, `0b085b1`, 2026-04-10/11).** The
parallel_classify_regression investigation discovered that every header
scan running *after* PASS1's parallel allocator work was redundant -
`collect_pass1_generic` already scans the whole file once. By plumbing
`full_way_schedule` and `pass3_blob_schedule` out of `collect_pass1_generic`
via `Pass1Result` and consuming them via `mem::take` in PASS2/PASS3, smart
extract now does ONE file scan instead of THREE. Europe impact at
commit `cadc3e6` vs pre-investigation `fc17b51`:

| Strategy | Pre-investigation | Post | Δ |
|---|---|---|---|
| smart | 208.2s (`fc17b51`) | **181.4s** | **−13%** (−29% vs mid-investigation `5ca2df9` peak of 254s) |
| complete | 198.0s (`fc17b51`) | **164.9s** | **−17%** |
| simple | 113.1s (`fc17b51`) | **96.3s** | **−15%** |

Complete benefits because `extract_complete_ways` PASS2 now also consumes
`pass3_blob_schedule` via `pread_write_pass_with_schedule`. Simple benefits
from shared instrumentation and scan-path improvements in the same commit
range.

Europe simple phase breakdown (commit `b95e5ab`):
- Node classify: 13s, Node write: 11s
- Way classify: 6s, Way write: 40s
- Rel classify: 13s, Rel write: 2s

Historical: sorted pass1 optimization (commit `37b7c19`) impact on simple:
Denmark -14% (2625→2259ms), Japan -8% (12,619→11,643ms). Single-pass
classification on sorted input eliminates the second file read. Superseded
by the parallel classify + raw frame passthrough architecture.

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

First full breakdown after the 2026-04-17 instrumentation landing
(commit `1e8d37b`) added `MULTI_EXTRACT_START/END`,
`MULTI_SCHEDULE_SCAN_START/END`, and eight `multi_extract_*` counters.
The schedule-scan phase was invisible in sidecar `--durations` before
this.

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
92 % of wall. These are the real optimization targets. `SCHEDULE_SCAN`'s
26 s is the `BlobReader::seek_raw` BufReader-discard issue (see
TODO.md Performance section) showing up; ~10× amplification vs raw file
size at the default 256 KB buffer.

Counters emitted (UUID `c1ff6ec9` values):
- `multi_extract_region_count=5`
- `multi_extract_node_blobs`, `multi_extract_way_blobs`,
  `multi_extract_relation_blobs` (schedule sizes)
- `multi_extract_nodes_written`, `multi_extract_ways_written`,
  `multi_extract_relations_written` (cross-region totals)

### Way-classify scratch reuse (commit `b7cd0e4`, 2026-04-17)

The way-classify phase used `|| ()` as its per-worker init and
allocated `vec![Vec::<i64>::new(); n]` fresh inside the classify
closure on every blob, letting each inner `Vec<i64>` grow through
repeated doublings on each block rather than amortizing capacity
across the ~N blobs each decode worker processes. Fix swapped to the
same pattern the node classify phase already uses (per-worker scratch
cleared-not-dropped between blocks, drained into the return value).

| Scope | WAY_CLASSIFY pre-fix | WAY_CLASSIFY post-fix | Δ |
|---|---:|---:|---:|
| Japan 5-region | 892 ms (`8bc1773f`) | 848 ms (`08fefe51`) | **-44 ms / -5 %** |
| Europe 5-region | (no paired pre-fix bench) | 13,675 ms (`c1ff6ec9`) | - |

Japan phase delta is within the 5-10 % range expected from the
mechanism (fewer growth reallocations per inner `Vec<i64>`). Europe
was not paired-benched because the targeted phase is only 1.7 % of
wall - a near-perfect phase speedup would still be within single-shot
noise on total. No regression on either scale. The fix is of interest
at planet scale only if multi-extract becomes a recurring workload.

## Tags-filter

Two-pass architecture: pass 1 classifies blobs in parallel (parallel
classification + lightweight scanner), closure + way dep scans also
parallelized via `parallel_classify_phase`, pass 2 writes matching
elements.

| Dataset | Sequential (old) | Two-pass (pass 1 only) | Two-pass (all parallel) | Commit |
|---------|-----------------|------------------------|------------------------|--------|
| Europe (33.6 GB) | 363s | 158s | **107.5s** (-70%) | latest |

Previously OOM with pipelined reader. Sequential fix (commit `2a8a649`)
brought it to 363s. Parallel classification for pass 1 brought it to
158s. Parallelizing closure + way dep scans brings the total to 107.5s.
Full journey: 366.7s → 107.5s (3.4x total improvement).

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

### ALTW external optimization arc (post-3d977a0)

Cumulative effect of the four landed seam deletions in
`notes/altw-structural-reports.md` (#8 router, #4 stage-2 de-ranking,
#9 L1 metadata-driven relation scan, plus their predecessors).

| Commit | Change | Europe | Planet |
|--------|--------|-------:|-------:|
| `3d977a0` | Pre-structural-reports baseline | 400s | 953s |
| `4f059b67` | (pre-#8 planet baseline in structural reports) | - | 867.7s |
| `d3e13ed` | (pre-#8 Europe baseline in structural reports) | 333s | - |
| `e497e54` | #8 `BlobLocationRouter` (finalize consolidation removed) | 320.5s [TAINTED] | - |
| `f1a4ada` | #4 stage-2 blob-local rank counter + drop rank index | 308.0s [TAINTED] | - |
| `6d71053` | #9 L1 metadata-driven relation scan | 291.6s [TAINTED] | - |
| `7904a95` | (current, #3/#11 attempted and reverted - bench `123f70f1`) | 291.6s [TAINTED] | **698.1s** [TAINTED] |

Planet drop `867.7s → 698.1s` (**−19.5%**) confirms the
stage-2/relation-scan wins scale more strongly with tuple count than
the Europe numbers suggest. Phase deltas vs `4f059b67` planet baseline:
stage 1 `148.5s → 112.8s` (−24%), stage 2 `266.6s → 235.2s` (−12%),
stage 3 `100.2s → 85.7s` (−14%), finalize/router `46.4s → 1.4s` (−97%,
all of #8), relation scan down to 6.0s (#9 L1), stage 4 `231.6s →
215.6s` (−7%).

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

### Optimization arc (Denmark, plantasjen)

| Commit | Change | Time | Cumulative |
|--------|--------|------|------------|
| `d27f17e` | Baseline (4 scans, sequential for_each) | 21.4s | - |
| `e7a12e6` | 3 scans (reorder: relations first) | 18.5s | -14% |
| `da4d939` | 2 scans (fused node+way, pipelined) | 10.9s | -49% |
| `60df011` | Zero-alloc cover_segment + parallel S2 cells | 10.4s | -51% |
| `398b1a4` | Block-pipelined, skip_metadata, tag-first way classification | 9.7s | -55% |

### Germany RSS profile (commit `3449db2`, plantasjen, hotpath)

588s total, 3.6 GB peak RSS. Per-phase memory:

| Phase | RSS | Wall time | Notes |
|---|---|---|---|
| After pass 1 (relations) | 223 MB | 1.8s | admin_relations + IdSetDense |
| After pass 2 scan (nodes+ways) | **17.6 GB** | 572s | Dense node index mmap dominates |
| After pass 2 drop (node index freed) | 168 MB | - | Pages evicted, data Vecs are modest |
| After ring assembly | 428 MB | +12.7s | + admin polygons (43K) |
| After interpolation resolution | 955 MB | +4.4s | + transient spatial index |
| After cell assignment | **3.7 GB** | +10s | All cell entry Vecs materialized |

Pipeline (`run_pipeline`) takes 556s / 94% - Germany is I/O + decompress bound
at this scale. Main thread CPU averages 32% (waiting on pipeline).

Key observations for planet-scale planning:
- Dense node index is the RSS peak (17.6 GB). Planet would push to ~30+ GB.
  Referenced-node-only index (pass 1.5 in planet spec) would cut this to ~10 GB.
- Cell entry Vecs are the second peak (3.7 GB). Planet estimate: ~19 GB.
  Bucketed cell assignment (planet spec) eliminates this.
- Data Vecs (streets, addr, interp, strings) are only ~168 MB after node index
  drops. Streaming to output files would reduce this further but is not the
  bottleneck at Germany scale.

### Comparison with traccar-geocoder

No directly comparable data - different hardware, different format, different
build architecture (traccar uses C++ with libosmium, single-threaded, all data
in RAM). Numbers from the HN thread (2026-03-21):

| Dataset | traccar-geocoder | pbfhogg | Notes |
|---------|-----------------|---------|-------|
| Australia/Oceania (~1.1 GB) | ~15 min (KomoD) | - | Not tested |
| Germany (4.5 GB) | - | **30.9 s** | After 2026-04-18 optimization arc (was 30 min, then 9.8 min) |
| Planet (~87 GB) | 8-10 hours (192 GB RAM) | **7m12s** (27 GB host) | After 2026-04-18 optimization arc |

Planet (validated, commit `82db8ed`, UUID `b4b25c05`, 2026-04-18,
plantasjen, `--bench 1`): **432.9 s (7m12s), ~25 GB peak anon RSS** in
`GEOCODE_PASS3_STAGEB_FINE`. Against the historical
`7e9c2e9` baseline of 1,255 s / 29.5 GB peak at `GEOCODE_PASS1_5`
[TAINTED - wall inflated by all-blobs-scan regression; RSS unaffected],
that's **-65.5 % wall** and **-14 % peak anon** (the governing peak
moved from Pass 1.5 to Pass 3 Stage B, Pass 1.5 itself dropped from
29.5 GB to 3.0 GB via the shared-atomic `IdSetDense` swap).

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

**Sequential (pre-parallel landings).** Commit `7e9c2e9`, `--bench 1`.
Both rows sit inside the 2026-04-18 TAINTED window
(`4ce7e93..c0ae9a7`) - wall carries the `has_indexdata` O(N)
all-blobs-scan cost that was fixed in `aa3147c`; RSS and sub-phase
data are unaffected.

| UUID | Args | Wall | Peak anon RSS |
|---|---|---:|---:|
| `42aedca1` | `--from base --to 20260411` (default text summary) | 2150.9 s (35m51s) | - |
| `53900d5f` | `--from base --to 20260411 --format osc` | 2225.6 s (37m06s) | **54.9 MB** |

The sequential path is a streaming merge-join between the two sorted
PBF readers - no bulk in-memory structures - so peak anon stays tiny
(~55 MB) regardless of input size. Single-threaded at 1.0 avg cores;
229 GB disk read + 15 GB OSC output write. Safe on any host.

**Post-parallel (shard-based block-pair merge).** Commit `06628d8`,
2026-04-20, `--bench 1`. Opt-in via `-j/--jobs N` on `pbfhogg diff`;
both text and `--format osc` paths are parallelised over the same
ID-range shard plan.

| UUID | Args | Wall | Peak anon RSS | vs sequential |
|---|---|---:|---:|---:|
| `b02d86bc` | `--from base --to 20260411 -j 16` (text) | **208.6 s (3m28s)** | 2.29 GB | **10.2× faster** |
| `9b3fc2b9` | `--from base --to 20260411 --format osc -j 16` | **313.8 s (5m13s)** | 663 MB | **7.1× faster** |

Peak anon diverges by design: text shards buffer their formatted
output in an in-memory `Vec<u8>` until all workers finish, then the
main thread concatenates to stdout. OSC shards stream XML fragments
straight to per-shard scratch temp files (`BufWriter<File>`), so peak
anon is just in-flight decoded blocks. OSC's serial `assemble_osc`
(gzip + concat of ~45 GB of XML fragments) is 32.8 s / 10 % of wall
and is the main reason its speedup is lower than text's.

Phase split (post-parallel planet), both paths: NODE dominates
(~73 %), WAY second (~26 %), REL rounding error. Walker phase is now
~15 s (pread-only via `HeaderWalker` with `posix_fadvise(RANDOM)`;
disk read dropped from 45 GB to 2.6 GB). Shard balance on germany is
within 1.03× max/min across all three type phases.

Avg cores (planet text `-j 16`): NODE 14.7, WAY 12.9, REL 14.5 out of
16 (86-92 % utilization). Peak threads 17 = 16 workers + 1 main.

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
