# Performance History

Durable record of optimization arcs, phase breakdowns at landed-then-superseded
architectures, regression-window retrospectives, and old commit-pinned
cross-dataset tables. The current state of each command lives in
[performance.md](performance.md); this file is the "how we got here"
companion. Plan docs in `notes/` are working scaffolding and get deleted
when the work lands - this file is where their durable findings settle.

## TAINTED window (2026-04-09 to 2026-04-17, retired 2026-04-27)

A `has_indexdata` / `check_sorted_and_indexed` regression in the commit range
`4ce7e93..c0ae9a7` (Apr 9-17 2026) added an unaccounted O(N) all-blobs-scan
cost to commands whose hot path walks blob headers. Affected commands:
`add-locations-to-ways`, `build-geocode-index`, `cat --type`/`--dedupe`,
`check-ids`, `diff`, `extract` non-simple, `getid`, `inspect --nodes`/`--tags`,
`sort`, `tags-filter` non-invert. Other commands during that window were
unaffected.

The fix arc landed across `aa3147c` (`BlobReaderSource` `BufReader::seek_relative`
override), `ca6711e` (`has_indexdata` short-circuit), and the broader
`HeaderWalker` adoption sweep `de8daf1`..`01c67da`. Per-command headlines from
that arc:

- `cat` planet 497 s → 86.5 s (5.8×, both regression rollback and substantial
  improvement on the pre-regression path).
- `getid` planet 44 s → 6.1 s (7.2×, via shared `pread`-only `HeaderWalker`).
- `inspect` default planet 21.4 s → 6.5 s (3.3×).
- ALTW external europe -15.6 s / -5.5 % (Meta scan -49 % from the
  `BufReader::seek_relative` preservation; small downstream gains in
  Stages 1+2 from page-cache benefit).

Tainted DB rows were invalidated 2026-04-27 once each axis had either a
post-fix replacement in `.brokkr/results.db` or was waived as no-longer-active
(stage-isolation diagnostics, ALTW alt-compression study, sequential
diff-snapshots osc, cat-from-raw-input planet). The audit script at the time
was `notes/audit_tainted_runs.py` (now retired).

---

## Cat passthrough (planet 497s → 86.5s arc)

The historical planet row (497s / 8m17s buffered at `69a127f`) measured a
combined cost: sequential decompress+reframe plus the `has_indexdata` /
`check_sorted_and_indexed` O(N) all-blobs-scan regression that was live
through `4ce7e93..c0ae9a7`. The seek-raw fix (`aa3147c`, buffer-preserving
`BlobReaderSource` header walk) and the short-circuit fix (`ca6711e`)
together drop the planet run from 497s to 86.5s - a 5.8× improvement at
the same output shape. Passthrough remains buffered-only; `--direct-io`
adds alignment overhead without the concurrent read/write pattern that
makes it faster for merge.

---

## Multi-dataset throughput tables (commit-pinned)

### Read throughput (commit `d387301`)

Count all elements, best of 3 runs.

| Dataset | Size | sequential | parallel | pipelined | blobreader | mmap |
|---------|------|-----------|----------|-----------|------------|------|
| Malta | 9 MB | 49 ms | 9 ms | 24 ms | 50 ms | 52 ms |
| Denmark | 487 MB | 2.86s | 463 ms | 1.46s | 2.93s | 2.93s |
| Norway | 1.4 GB | 8.4s | 1.33s | 4.9s | 8.9s | 8.8s |
| Japan | 2.4 GB | 14.5s | 2.1s | 8.0s | 15.2s | 15.2s |
| Germany | 4.7 GB | 26.9s | 4.2s | 13.0s | 27.8s | 27.6s |

North America (18.8 GB, 2.58B elements, commit `a6ebbfe`):
parallel 22s, pipelined 57s, sequential 130s.

### Write throughput (commit `d387301`)

Decode all elements then write through BlockBuilder + PbfWriter to `/dev/null`.

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

---

## Merge (apply-changes)

### skip_metadata in block_overlaps_diff (commit `b90e8ef`)

Single-line change: use `elements_skip_metadata()` in `block_overlaps_diff`
(only accesses element IDs, not metadata).

Germany hotpath (commit `1b10f18`, plantasjen):
- apply-changes-zlib: **6942ms → 5928ms (-15%)**

Larger improvement than expected - Germany's 18.4% rewrite fraction means
more blobs reach the precise `block_overlaps_diff` check (which decodes all
elements to test IDs against the diff). Skipping metadata decode saves ~1s
across ~11K precise-check invocations.

### Descriptor-first streaming pipeline (P1 + P1.5 + parallel pwrite, 2026-04-21, commits `719f306` → `80b37df`)

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

Pool-size sweep at cross-disk zstd:1 (plantasjen, Samsung 990 PRO):
4 → 89.2 s, 8 → 83.4 s, 16 → 80.9-82.1 s (two runs), 32 → 82.2 s.
POOL_SIZE=16 is hard-coded in `src/write/parallel_writer.rs`. Over-contends
above 16.

---

## Add-locations-to-ways

### External join stage breakdown (Europe, commit `d3e13ed`, plantasjen)

Shared blob-metadata pass replaced three earlier separate scans
(`s1_way_schedule_build_ms` 24.8 s → 0.08 s,
`s1_node_map_build_ms` 30.9 s → 0.12 s,
`s4_schedule_scan_ms` 31.5 s → 0.14 s), collapsing ~87 s of repeated
work into ~31 s of shared scan.

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

Later at `e497e54` the finalize phase was replaced by an in-RAM
`BlobLocationRouter` routing table (finalize 18.3 s → 0.163 s on a
single-sample bench), trading a consolidated `coord_payloads` file for
95 MB of encoded straddler bytes in RAM plus ~21 GB of worker tmps kept
open across stage 4. See [notes-archive equivalent in commit history]
for the superseded `3d977a0` and `e497e54` breakdowns.

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

| Caller / Command | Dataset | Pre-fix `ca6711e` | Post-fix `aa3147c` | Δ |
|---|---|---:|---:|---:|
| extract --smart | Europe | 211.2 s | 195.2 s | **−16.0 s, −7.6 %** |
| add-locations-to-ways --index-type external | Europe | 286.3 s | 270.7 s | **−15.6 s, −5.5 %** |
| add-locations-to-ways --index-type external | Planet | (skipped) | 700.6 s | within noise |
| tags-filter | Europe | 91.7 s | 93.1 s | within noise (+1.5 %) |
| renumber | Planet | 218.6 s | 206.7 s | **−11.9 s, −5.4 %** |

### Europe ALTW phase breakdown post-seek_raw

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

Meta scan delta is the direct effect of `BufReader::seek_relative`
preserving the buffer on every blob-body skip; the small downstream
deltas in stages 1+2 are page-cache benefit (header walk used to amplify
reads ~10× and push subsequent stages' working set out of cache).

### External join planet (commit `aa3147c`, post-`seek_raw` fix)

Planet wall 700.6 s (UUID `e30f7ddc`, `--bench 1`), basically identical
to the pre-regression baseline of 698 s (within single-shot variance).
META_SCAN measures 17.5 s post-fix - vs the inferred ~30 s pre-fix on
the same code path, so the seek_raw fix saves ~12 s in that phase. As a
fraction of planet total wall (700 s), that's 1.7 %, comfortably inside
the noise floor of `--bench 1`.

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

Planet `024422c..e30f7ddc`: original 256× re-read shape on Denmark was
302 s. Europe traced 2,060 s → 320 s across the intermediate landings
(single-pass merge, scatter buffer, P2b/P2c parallel assembly, external
radix, coord_payloads integration, shared blob metadata scan,
BlobLocationRouter).

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

A1 (rankless node-ID bucketed stage 1+2, landed 2026-04-25) cut europe
from 291.6 s at `6d71053` to 270.8 s at `0dc8ae1` (-7.1 %) by replacing
the two-pass way scan + rank index with single-pass IdRecord emission
and a streaming merge-walk.

---

## Check commands

### check --refs (commits `8f0ccbb`, `053def6`, `fbf591c`, 2026-04-17)

Sequential main-thread consumer pegged at 100 % CPU on `RoaringTreemap::insert`
pre-swap. Step #1 swapped to `IdSetDense` (O(1) insert/contains, purpose-built
for dense-monotonic OSM IDs). Step #2 parallelized via `build_classify_schedules_split`
(one header walk, per-kind schedules) + three `parallel_classify_phase` phases.
The `roaring` crate dependency was dropped entirely from pbfhogg after
these landed.

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

### check --ids --full (commits `855b3b2`, `0d71b3b`, 2026-04-17)

Only remaining `roaring` consumer before the swap. Streaming mode (default
`check --ids`) was constant-memory and unchanged; `--full` mode allocated
per-type `RoaringTreemap`s for duplicate detection. Swap mirrors
check-refs step #1 (adds `IdSetDense::set_if_new` / `set_atomic_if_new`
methods). Parallel rewrite mirrors check-refs step #2; uses the widened
`parallel_classify_phase` merge signature (`FnMut(usize, R)`) for
seq-ordered cross-blob monotonicity checks.

Post-fix planet re-bench (commit `ef6ce09`, 2026-04-18, UUID `c498fff0`,
`--bench 1`): 69.5 s / 1m10s, carrying both the `ca6711e` short-circuit
fix and the `aa3147c` `BlobReaderSource` seek-raw fix.

Planet phase breakdown:

| Phase | Wall |
|---|---:|
| pre-schedule setup (`ElementReader::open` + 3× `IdSetDense::pre_allocate` ~1.86 GB) | 22.2 s |
| SCHEDULE_SCAN_LOOP (one-pass header walk, 92 GB file) | 16.8 s |
| VERIFYIDS_NODES parallel scan | 36.6 s |
| VERIFYIDS_WAYS parallel scan | 17.0 s |
| VERIFYIDS_RELATIONS parallel scan | 0.5 s |
| **Total** | **93.1 s** |

Planet memory:

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

---

## Renumber

### Optimization history

Planet 3,456 s → 194 s (-94 %) across ~30 commits from `e156e97` to
`cb99106`. Key landings: work-stealing dispatch (resolved two
intermediate OOM kills at ~26 GB anon RSS), DenseNodes + way wire-format
rewriters, `IdSetDense` rank fusion (collapsed stages 2a+2b+2c), fused
R2A/R2B/R2d relation pipeline, atomic index dispatch, shared-atomic
`IdSetDense` with concurrent `AtomicU8::fetch_or` pass 1 writes. Each
commit verified on Denmark via `brokkr verify renumber` (306-relation
orphan delta preserved exactly).

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways /
14,124,889 relations / 12,435,459,911 way refs (stable across the
optimization arc).

The intermediate `6165394` breakdown with stages 2a/2b/2c that totaled
1,092 s / 18.2 min was preserved while the architecture matched; that
breakdown is now obsolete - `IdSetDense` rank fusion collapsed the
multi-stage 2a/2b/2c into a single rank-fused stage and the older
breakdown no longer maps onto the code.

---

## Extract

### PASS1 schedule reuse (commits `d4ea760`, `0b085b1`, 2026-04-10/11)

The parallel_classify_regression investigation discovered that every
header scan running *after* PASS1's parallel allocator work was
redundant - `collect_pass1_generic` already scans the whole file once.
By plumbing `full_way_schedule` and `pass3_blob_schedule` out of
`collect_pass1_generic` via `Pass1Result` and consuming them via
`mem::take` in PASS2/PASS3, smart extract now does ONE file scan instead
of THREE. Europe impact at commit `cadc3e6` vs pre-investigation
`fc17b51`:

| Strategy | Pre-investigation | Post | Δ |
|---|---|---|---|
| smart | 208.2s (`fc17b51`) | **181.4s** | **−13%** (−29% vs mid-investigation `5ca2df9` peak of 254s) |
| complete | 198.0s (`fc17b51`) | **164.9s** | **−17%** |
| simple | 113.1s (`fc17b51`) | **96.3s** | **−15%** |

Complete benefits because `extract_complete_ways` PASS2 now also consumes
`pass3_blob_schedule` via `pread_write_pass_with_schedule`. Simple benefits
from shared instrumentation and scan-path improvements in the same commit
range.

### Europe simple phase breakdown (commit `b95e5ab`)

- Node classify: 13s, Node write: 11s
- Way classify: 6s, Way write: 40s
- Rel classify: 13s, Rel write: 2s

### Historical (sorted pass1 optimization, commit `37b7c19`)

Impact on simple: Denmark -14% (2625→2259ms), Japan -8%
(12,619→11,643ms). Single-pass classification on sorted input
eliminates the second file read. Superseded by the parallel classify +
raw frame passthrough architecture.

---

## Multi-extract

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

---

## Tags-filter

### Sequential → two-pass → parallel arc

Two-pass architecture: pass 1 classifies blobs in parallel (parallel
classification + lightweight scanner), closure + way dep scans also
parallelized via `parallel_classify_phase`, pass 2 writes matching
elements.

| Dataset | Sequential (old) | Two-pass (pass 1 only) | Two-pass (all parallel) |
|---------|-----------------|------------------------|------------------------|
| Europe (33.6 GB) | 363s | 158s | **107.5s** (-70%) |

Previously OOM with pipelined reader. Sequential fix (commit `2a8a649`)
brought it to 363s. Parallel classification for pass 1 brought it to
158s. Parallelizing closure + way dep scans brings the total to 107.5s.
Full journey: 366.7s → 107.5s (3.4x total improvement).

---

## Pipeline end-to-end

### ALTW external optimization arc (post-`3d977a0`)

Cumulative effect of the landed seam deletions (#8 `BlobLocationRouter`
`e497e54`, #4 stage-2 de-ranking `f1a4ada`, #9 L1 metadata-driven
relation scan `6d71053`, plus their predecessors).

| Commit | Change | Europe | Planet |
|--------|--------|-------:|-------:|
| `3d977a0` | Pre-structural-reports baseline | 400s | 953s |
| `4f059b67` | (pre-#8 planet baseline in structural reports) | - | 867.7s |
| `d3e13ed` | (pre-#8 Europe baseline in structural reports) | 333s | - |
| `e497e54` | #8 `BlobLocationRouter` (finalize consolidation removed) | 320.5s | - |
| `f1a4ada` | #4 stage-2 blob-local rank counter + drop rank index | 308.0s | - |
| `6d71053` | #9 L1 metadata-driven relation scan | 291.6s | - |
| `7904a95` | (current, #3/#11 attempted and reverted - bench `123f70f1`) | 291.6s | **698.1s** |

Planet drop `867.7s → 698.1s` (**−19.5%**) confirms the
stage-2/relation-scan wins scale more strongly with tuple count than
the Europe numbers suggest. Phase deltas vs `4f059b67` planet baseline:
stage 1 `148.5s → 112.8s` (−24%), stage 2 `266.6s → 235.2s` (−12%),
stage 3 `100.2s → 85.7s` (−14%), finalize/router `46.4s → 1.4s` (−97%,
all of #8), relation scan down to 6.0s (#9 L1), stage 4 `231.6s →
215.6s` (−7%).

### Pipelined-reader decode-admission bound (`a0a2e3b`, 2026-07-10, plantasjen)

The 3-stage pipelined reader's stage-2 dispatcher used to `rayon::spawn`
per blob without backpressure, and the reorder buffer admitted far-ahead
blocks unboundedly - so decoded-block memory grew with file size, not
with the nominal `read_ahead`/`decode_ahead` caps (elivagar measured a
reorder high-water of 660 and a 21.5 GB RSS peak on a 19 GB North
America run; the same unfiltered-full-scan shape was live in pbfhogg's
own `time-filter` and `altw` sparse residual paths). Fix: an
`AdmissionGate` with release-after-deliver permits - a permit rides each
decoded item through the channel and reorder slot, so
`admitted - delivered <= decode_ahead` is a hard cap on live decoded
blocks. Landed `a0a2e3b`; validated same day, all gates kept:

| Gate | V0 (`86a03f2`) | V1 (`a0a2e3b`) | Bound | Result |
|------|---------------:|---------------:|-------|--------|
| read japan pipelined | 7.78 s | 7.48 s | <= x1.03 | PASS (−3.9%) |
| read europe pipelined | 110.2 s | 95.9 s | <= x1.05 | PASS (−13%) |
| getid europe `--add-referenced` | ~73 s | 74.3 s | <= x1.03 | PASS (+1.8%) |
| tags-filter europe `-R` | 19.59 s | 19.8 s | <= x1.03 | PASS (+1.1%) |
| memory: reorder filled high-water | unbounded (660 observed) | 32 | <= 64 | PASS |
| backpressure engaged | - | `decode_admit_blocked=10216` | > 0 | PASS |
| denmark smokes | fast | fast | no blowup | PASS |

Provenance: V1 rows are in `results.db` at `a0a2e3b` (japan read
`ba5e1f0a`, europe read `7f35648f`, getid `cc846f0c`, tags-filter
`4241316a`); V0 *read* baselines were captured post-hoc via
`brokkr read --commit 86a03f2` (japan `3285d2af`, europe `e3c62000`).
The V0 getid/tags-filter numbers were never stored as DB rows - they
survive only in the validation-session log and orphaned sidecar
artifacts, so treat those two baselines as approximate.

Counter-semantics change that rides along: `pipeline_reorder_high_water`
now counts FILLED reorder slots (the memory diagnostic); the old
window-length meaning (the completion-skew diagnostic) moved to
`pipeline_reorder_window_high_water`. Cross-run comparisons against
pre-`a0a2e3b` UUIDs (e.g. the 660) must use the window column. New
counters `pipeline_decode_admit_wait_ns` / `pipeline_decode_admit_blocked`
capture the read backpressure that `raw_send_wait_ns` structurally never
saw (stage 2 never let the raw channel fill).

Deliberately deferred alternative: exporting the
`scan::classify::parallel_classify_phase` machinery as `pub` API (so
external consumers could adopt the bounded-by-construction pread-worker
shape) was weighed and postponed - the classify surface is still moving
in-tree, and `pub` is a hard-to-walk-back contract. Revisit only if
elivagar proves classify is its long-term primary read path.

---

## build-geocode-index

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

Key observations from the planet-scale planning of this measurement:
- Dense node index was the RSS peak (17.6 GB). Planet would push to ~30+ GB.
  The shared-atomic `IdSetDense` swap (referenced-node-only index) cut this
  dramatically; current planet Pass 1.5 peak is 3.0 GB.
- Cell entry Vecs were the second peak (3.7 GB). Planet estimate at the time:
  ~19 GB. Bucketed cell assignment eliminated this.
- Data Vecs (streets, addr, interp, strings) were only ~168 MB after node index
  drops. Streaming to output files would reduce this further but was not the
  bottleneck at Germany scale.

---

## CLI commands (commit-pinned multi-dataset tables)

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

### Germany hotpath (4.7 GB indexed, ~496M elements, commit `1b10bfd`, plantasjen)

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

### vs osmium (Denmark, commit `23862d1`)

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| sort (sorted, indexdata) | **0.14s** | 11.6s | **83x** |
| apply-changes (indexdata + zlib) | **2.7s** | 7.2s | **2.7x** |
| tags-filter w/highway=primary -R | **0.24s** | 0.56s | **2.3x** |
| cat --type way (indexdata, raw passthrough) | **0.24s** | 2.22s | **9.3x** |
| add-locations-to-ways | **8.3s** | 12.6s | **1.5x** |
| check --refs | **4.8s** | 4.5s | 0.94x |

---

## Env-gated read-path batch (2026-07-12 overnight, commit `a65cecc`, plantasjen)

Five read-path follow-ups from the 2026-07-11 architecture reports
(`notes/read-path-architecture-reports.md`) were landed behind default-off
`PBFHOGG_*` env gates and adjudicated in one overnight A/B run - one binary,
one commit, gate-off IS the baseline, same-day A/B by construction. Host
plantasjen (23 GB RAM this run; the reports' "26-30 GB" was swap-inclusive).
Pre-registered noise floor +/-3%. Working read-out lived in
`notes/env-gated-readpath-batch.md` (deleted on landing); this is the
durable record.

| Item | Gate | Verdict | Key numbers |
|---|---|---|---|
| 1 fadvise DONTNEED watermark | `PBFHOGG_FADVISE_BATCH_BYTES` | REVERT | planet-8k all modes +0.75% to +2.68%, inside noise, mildly slower |
| 2 byte-aware buffer knobs (x4) | `READ_AHEAD/DECODE_AHEAD/BLOCK_QUEUE/CMD_BATCH_BYTES` | REVERT | 8k pipelined +3.13%, CMD_BATCH getid-8k +3.49% regressed; no win anywhere |
| 3 command-transform fusion | `PBFHOGG_FUSE_TRANSFORM` | **KEEP** | getid-8k -7.68%, getparents-8k -6.51%, tags-filter-R 8k -6.97%; getid-primary GETID_PASS2 RSS 1.18 GB -> 596 MB (-50%) |
| 4 ordered batched pipeline | `PBFHOGG_BATCHED_PIPELINE` | REVERT | getparents-8k -6.67% (redundant with fusion); combination getid-8k +9.30% regression |
| 5 europe prefetch WILLNEED | `PBFHOGG_PREFETCH_WILLNEED` | **KEEP** | check-refs europe -6.16%, tags-filter europe -5.66% |

**Failed experiments, recorded so they are not blindly re-attempted:**

- **Ordered batched-pipeline rebuild (item 4)** - a byte-bounded ordered
  batch engine with long-lived workers, both architecture reports'
  "high-conviction" rewrite. Measured: no isolated win. Its one
  floor-clearing cell (getparents-8k FullScan -6.67%, base `357c360f`
  62.0 s -> `53a9e76a` 58.8 s bench-3) is redundant - fusion delivers the
  same win on the same cell. Reads neutral (pipelined 8k +2.10%, primary
  +1.59%, europe -1.40%), getid-8k isolation +0.05%, tags-filter-R 8k
  -0.65%. The both-gates combination (BATCHED+FUSE getid-8k, `f1d76362`)
  REGRESSED +9.30% vs baseline (197.9 -> 216.3 s) - and +18.4% vs
  fusion-alone (182.7 s, `895184ee`), the figure that actually kills the
  candidate since fusion-alone is the shipped end state - with deeper
  reorder skew (`pipeline_batched_reorder_high_water` 32 vs 14,
  `pipeline_batches` 43564 vs 30977). Diagnosis: at 1.45 M blobs the per-blob channel/task/
  permit/reorder seams cost LESS than the reports estimated, and the batch
  engine's own per-batch coordination gave the difference back. Do not
  re-propose a batch-rebuild of `run_pipeline` on "per-blob seam overhead
  x blob count" reasoning without first measuring that the seams are the
  binding cost - here they were not.
- **fadvise DONTNEED watermark batching (item 1)** - watermark-batched
  eviction to replace the per-blob cumulative-prefix `posix_fadvise`. The
  1.45 M advisory syscalls on the 8k encoding are real but carry no
  measurable wall cost; every planet-8k mode came out inside +/-3% and
  slightly slower gated. Mispriced (user confidence was already zero).
- **byte-aware buffer knobs (item 2)** - making `read_ahead`/`decode_ahead`/
  `BLOCK_QUEUE`/command-batch admission byte-primary instead of count-primary.
  No configuration improved anything; two knobs regressed just past the
  floor. Count-vs-byte admission was not the binding constraint at either
  encoding.

**Kept (promoted to default; current numbers in `performance.md`):**

- **Command-transform fusion (item 3)** - moving getid pass-2, getparents
  FullScan, tags-filter `-R`, and altw decode-all transforms INTO the decode
  workers, deleting the 64-block (~90 MB) materialization + second rayon
  dispatch. All three surviving signal cells cleared +3% (getid-8k
  `a57807df` 197.9 -> `895184ee` 182.7 s; getparents-8k `357c360f` 63.0 ->
  `f461f307` 58.9 s; tags-filter-R 8k `0aa45689` 45.9 -> `896b8ffc` 42.7 s,
  all bench-3). Executing no-regression controls also improved: getid-primary
  -12.88% with GETID_PASS2 peak RSS halved (1.18 GB -> 596 MB) and pass-2
  wall -17% at higher core occupancy; tags-filter-R primary -6.25%. The altw
  europe-raw signal pair OOM-killed on the 23 GB host (anon decode-all +
  re-encode), so altw fusion is correctness-proven but performance-unmeasured
  - a follow-up night on a bigger host owes it. Resolved as State 3 of the
  fusion/batching four-state matrix (keep fusion, revert batching), since the
  two share the `run_pipeline` seam.
- **Europe WILLNEED prefetch (item 5)** - `POSIX_FADV_WILLNEED` over the
  scan schedule's `(data_offset, data_size)` ranges, reclaiming the
  page-warming the 2026-04-20 HeaderWalker swap to `POSIX_FADV_RANDOM` gave
  up. The TODO estimate ("~14 s") was unbacked; the first real measurement
  is ~6% on both europe consumers (check-refs `a64f56dd` 56.8 -> `7d114432`
  53.3 s; tags-filter `9b47383c` 61.8 -> `c8681230` 58.3 s, bench-3).
  Europe-only: planet is larger than RAM so prefetched pages evict before
  reuse.

## Retired plan docs

These plan docs lived in `notes/` while their work was active and were
deleted on landing. Their durable findings are absorbed into the sections
above; the file names are preserved here as breadcrumbs for searching old
commit messages and PR descriptions.

- `notes/diff-snapshots-opportunities.md` (Tier 1 items landed; sharded
  parallel block-pair merge is the canonical implementation).
- `notes/getid-include-optimization.md` (HeaderWalker + 1-pread probe
  shape landed at `d263d76`).
- `notes/scan-optimization-audit.md` (Tier 1 items landed; dense node
  index Pattern 2, O(1) `check_sorted_and_indexed`/`has_indexdata`
  probes, and unsorted extract paths remain intentional non-goals).
- `notes/pipelined-reader-decode-backpressure.md` (admission gate landed
  `a0a2e3b`, validated same day; findings in "Pipelined-reader
  decode-admission bound" above; root cause + bound semantics live in
  `run_pipeline`'s doc comment and `pipeline_metrics.rs`).
- `notes/env-gated-readpath-batch.md`, `notes/fusion-spec.md`,
  `notes/pipeline-rebuild-spec.md` (the 2026-07-12 env-gated batch:
  plan of record plus the fusion and batched-pipeline specs; verdicts
  in "Env-gated read-path batch" above, fusion kept as ADR-0009, the
  batched engine and byte knobs reverted).
