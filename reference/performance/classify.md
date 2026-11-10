# Classify

### Blob-count threshold dispatch landing gates (ADR-0006, plantasjen, 2026-07-11)

getparents and getid include mode dispatch between the pread header
walker and a full-file scan at an estimated 150,000 OSMData blobs
([`decisions/0006`](../../decisions/0006-blob-count-threshold-dispatch.md)).
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
`reference/performance/read-pipeline.md` "Env-gated read-path batch".

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

Release builds gain nothing: `check --refs crates/lib/tests/test.osm.pbf` peaks at
7012 kB on `03e3792d` and 7052 kB on the parent (zsh `time` `%M`), so the
optimizer was already lowering the parent's `collect()` of zeros to a
zeroed allocation, and the floor pre-allocation was never resident there.
The 1.2 GB seen for a three-node `check --refs` came from a debug test
build, which keeps the word-by-word write. The change makes the zero pages
explicit rather than an optimization, which is what the debug-built test
suite gets. Even a small real extract spreads its IDs over the whole ID
history and writes most pages of most chunks, so extracts do not shrink
either.

### Ordered pool: notify only parked workers, passthrough ahead of its result (bygg, 2026-11-08, against `141bb4c4`)

Two changes in `crates/lib/src/ordered_pool.rs`. The admission gate's `consumed` is an
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

## Classify WILLNEED in honoured calls (bygg, 2026-11-09, baseline `6fdde3f4`)

Linux honours one `POSIX_FADV_WILLNEED` call only up to the larger of the
device's optimal I/O size and the readahead window (128 KiB on bygg's
nvme0n1) and drops the rest with a successful return. The classify pools'
sliding advice issued each newly exposed slice of its 256 MiB window as
one call, and on a file whose bodies coalesce into one range (planet,
germany) each slice spans MB, so only its first 128 KiB was read ahead.
Every measurement of the classify advice before this one, including the
neutral extract simple cells and the 1.4 % consumer cost above, was taken
with those truncated calls.

Change: `fadvise::will_need` is the one way to issue `WILLNEED` and splits
a range at absolute multiples of 128 KiB (`WILLNEED_CALL_BYTES`), so each
call's page-rounded extent stays inside one block the kernel honours whole;
the header walk's own chunk loop folded into it. Baseline = stored clean
`6fdde3f4` cells; variant = the uncommitted change with `--force` (not
stored, walls from the run output), after `sudo fstrim -av`, in A B B A
order. Each germany cell ran straight after a planet cell, so its input
was cold.

| Row | Baseline | Variant |
|---|---|---|
| planet `cat --clean` (ordered pool) | 137.7 (`66490c43`), 144.4 (`1fd2a61a`) s | 141.5, 140.6 s |
| germany `cat --clean`, cold | 8.34 (`e7250afc`), 8.39 (`ccc17685`) s | 8.27, 8.27 s |
| planet `check --refs` (unordered pools) | 31.13 (`968e63b3`), 31.09 (`21f51836`) s | 26.40, 26.36 s |

`fadvise_willneed_wait_ns`: planet `cat --clean` 2.2 / 2.1 s against
15.0 s, germany 0.40 against 0.46 s, planet `check --refs` 2.3 against
12.8 s. `check --refs` is 15 % faster, sign-consistent in both pairs: its
per-blob dispatcher now keeps the 256 MiB ahead of the workers actually in
flight. `cat --clean` moves within noise on planet and on cold germany; the
consumer thread spends seven times longer in the syscall without the wall
noticing, so it is still not what binds. Not measured: the other classify
commands (tags-filter, getid, getparents, extract, altw pass 0) and europe;
the extract simple neutral reading above predates the fix.

## History

### Parallel classify: columnar decode, accumulation shape, dispatch

Durable landing record for the columnar decode prototype and the
`parallel_classify_phase` / `parallel_classify_accumulate` accumulation
model. Absorbed from the columnar-integration and hybrid-batching research
notes on 2026-07-13 before those notes were deleted. The current
per-command state lives in `reference/performance/README.md`; this is the "how we got here" plus the do-not-reattempt pins.

#### Columnar dense-node decode (prototype `e0b0780`)

`DenseNodeColumns` (`crates/lib/src/read/columnar.rs`) batch-decodes IDs, lats, lons
into contiguous arrays. `collect_matching_ids_bbox` (single region) and
`collect_matching_ids_multi_bbox` (N regions) test each node against the
bbox set in one pass and push matches to a `Vec`. Shipped in single- and
multi-extract node classification. The `IdSetDense` output variants
(`set_matching_ids_bbox` / `set_matching_ids_multi_bbox`) were removed in
`c4c7b9e` as unused - see the accumulation finding below for why direct
set() lost to Vec push.

Multi-extract Japan 5-region node classify (commit `c3b271f`, plantasjen):
1081 ms -> 748 ms (-31%) columnar vs element-by-element; total 8.1 s ->
7.3 s.

ASM inspection confirms LLVM does NOT autovectorize the bbox loop - the
`push()` side effect blocks it, so explicit AVX2 intrinsics are the only
SIMD path. The multi-bbox loop is the designated SIMD target (N region
tests per node amortize setup); not yet built.

#### IdSetDense accumulation: Vec-push-then-drain wins (do-not-reattempt, `e94c3c8`)

Replacing the hot-loop `Vec::push` with a direct `IdSetDense::set()` (the
type has since been renamed `IdSet`) per
matched ID (commit `e94c3c8`, plantasjen) cut allocation hard but
regressed wall by an order of magnitude:

| Path | Vec::push | Direct IdSetDense::set() | Delta |
|---|---:|---:|---|
| Alloc (single-extract) | 8.7 GB | **20.5 MB** | -99.8% |
| Node classify | 713 ms | **20.5 s** | **29x slower** |
| Way classify | 943 ms | **3.7 s** | **4x slower** |

Root cause: `IdSetDense::set()` does a chunk lookup + byte offset +
bitmask per ID (random access); `Vec::push()` is a sequential L1-friendly
append. IDs arrive already sorted from the columnar decode, so the
randomness is in the chunk access pattern, not ID order. Verdict (2
perf reviewers): hybrid is correct - Vec push in the hot loop, drain once
into IdSetDense; direct `set()` is fine only for sparse paths (polygon,
relation, way deps) where filter work dominates and match counts are low.
A `set_sorted_batch()` would amortize the chunk lookup but still loses to
push on cold-line read-modify-write. Do not re-attempt direct set() in a
dense classify hot loop.

#### Accumulation is not planet-safe for dense paths (2026-04-09 design review)

A 10-reviewer design review (2026-04-09) settled the two-function split:
`parallel_classify_phase<S, R>` (per-blob `R` sends with persistent
scratch `S`) for dense paths, `parallel_classify_accumulate<S>`
(per-worker accumulation) for sparse paths. Per-worker Vec accumulation is
NOT planet-safe on dense paths - per-worker planet estimates: node
classify multi/5-region `Vec<Vec<i64>>` ~3.5 GB, way classify single
1.6 GB way + 9.5 GB refs, way classify multi/5-region 8 GB, tags_filter
pass 1 2.9 GB - all "per-blob send". Sparse paths stay on accumulate:
relation classify (3x IdSetDense) ~68 MB, tags_filter relation closure
~13 MB. Any remaining `parallel_classify_accumulate` caller is owed the same
per-worker planet bound before it is trusted at planet scale.

Note: the review's original *mechanism* diagnosis (a chunk-spread model
pinning the planet blocker on `extract.rs:2813`) was investigated over
four rounds 2026-04-10/11 and proved wrong end-to-end. The real mechanism
was a cold-arena-page residency cascade (post-PASS1 header scans touching
glibc's reserved-but-unpopulated pages), unrelated to chunk spread. The
architectural fix (`extract.rs:2813` -> per-blob send, `cc19d26`) was kept
anyway (correct in principle, -23% PASS2 wall) and the PASS1 schedule-reuse
landings (`d4ea760` PASS2, `0b085b1` PASS3) removed the triggering scans
for ~29% cumulative Europe smart-extract wall. Planet smart extract at
`cadc3e6` (UUID `2d028196`, Europe bbox): 11.17 GB peak anon / 279 s - see
the Extract section above and the `reference/performance/extract.md` Extract planet footnote.

#### Mutex<Receiver> blob dispatch is not a bottleneck (do-not-reattempt)

The shared `Mutex<Receiver>` single-recv-per-lock dispatch used by the
`crates/lib/src/scan/classify.rs` worker pools, tags-filter pass 2, and multi-extract
write workers was flagged by 4/6 reviewers (2026-03-29) as a suspected
~8 s regression in the pipelined-reader -> pread-worker conversion. Closed
2026-07-13 without a batch-drain: the arithmetic bounds total mutex time at
~50-500 ms at Europe scale (500K blobs x ~100 ns uncontended futex, even at
10x contention overhead), under 1% of wall, and no hotpath or sidecar
measurement since (getparents, sort, geocode, the 2026-07 blob-density
sweeps) has surfaced the dispatch lock as a cost. The "~8 s regression"
claim was never substantiated; the real conversion win was the Europe
tags-filter two-pass drop from 366 s (sequential BlobReader, `1e6e70c`) to
105 s (pread workers, `75ad21d`: pass 1 classify 34 s / closure+deps 33 s /
pass 2 write 37 s), a 3.4x improvement whose accounting is the tags-filter
arc above. Do not build a batch-drain over this dispatch on "one lock per
blob" reasoning without first measuring the lock is the binding cost. The
classify pools that keep the dispatch record it: workers queued on the lock
in `classify_unordered_desc_lock_wait_ns`, the holder waiting on an empty
channel in `classify_unordered_desc_recv_wait_ns`. The holder keeps the
lock across its receive, so a receive wait of t with n peers queued adds up
to n x t of lock wait: lock wait cannot be split into mutex cost and queue
starvation by subtracting the receive wait. Both time only the slow path
(a failed `try_lock`, an empty `try_recv`); uncontended acquisitions and
successful receives are invisible, so they are stall counters, not a
measure of the dispatch's whole CPU cost.

Measured 2026-11-09 from stored cells, no new runs: every planet extract
(simple, smart, complete) and tags-filter cell on bygg that carries the
counters (26 cells, stored between `003fe717` and `141bb4c4`) records 43-420 ms of
desc lock wait and 2-37 ms of desc recv wait per invocation, summed across
about 30 workers, on walls of 52-316 s. The worst, extract --smart
`a49fcc12` (`4a3ba718`, 132.6 s), splits per pool run (differences between
successive emissions) as 77 / 85 / 80 / 170 ms lock and 1.5 / 4.5 / 3.5 /
6.5 ms recv; the 80 ms run lasts under a second but is still summed over
the workers, not a serial pause. Descriptor blocking is negligible on these
workloads. This does not establish that an atomic-index claim or a batch
drain would cost the same, only that there is no measured reason to replace
the dispatch; an atomic claim would also have to re-home the dispatcher's
one advancing `WILLNEED` cursor.

---

### Worker-pool unification

Three looks at folding the hand-built blob pools into one engine, none
measured, recorded so a fourth starts from them.

- **March 2026, one blob pipeline: not done.** A consolidation review
  (removed in `8bcc00b6`, repeated in a note removed in `97a8d697`)
  catalogued every raw-frame pipeline of the time (cat,
  add-locations-to-ways, sort, merge) and ruled against a shared frame
  source and sink: orchestration differed per command, sort is random
  access, altw's two-phase read was unique, merge was bound to the diff
  overlay; upside estimated at best -2 %, downside +3 % to +12 %, never
  measured. It predates every pread-worker pool.
- **April 2026, pread-from-workers dedup: skipped** (introduced in
  `025a8b07`): two copies with different payloads and stats types, judged
  harder to read generic, "only after the pread API shape stabilizes".
- **October 2026, done in part.** Every pool that delivers in schedule
  order runs on `crates/lib/src/ordered_pool.rs`, and sort and `cat --dedupe` on one
  blob-merge engine. Of the unordered pools, the two per-blob classify
  pools share one driver (`run_unordered_per_blob`, `scan/classify.rs`),
  keeping their dispatcher thread because the measured `WILLNEED` advice
  (europe item 5 above) rides on it; the accumulate pool keeps its own
  loop, since per-blob send and per-worker accumulation must both survive.
  altw external stages 2 and 3 and geocode pass 1.5 stay hand-built
  claim-counter pools with purposeful differences (their comments say
  which), and the two altw way reframes share only their block-level walk
  (`altw::reframe::parse_way_block_top`): a coordinate-source trait was
  rejected because sparse resolves refs while stage 4 copies coordinates
  stage 3 already encoded. The unordered classify pools now record their
  blocking points as `*_wait_ns` stall counters, one process-wide set
  emitted after every run (`classify_unordered_dispatch_send_wait_ns`,
  `_desc_lock_wait_ns`, `_desc_recv_wait_ns`, `_result_send_wait_ns`,
  `_result_recv_wait_ns`; the accumulate pool records only the first three,
  as its result send cannot block and its consumer's wait is the scan
  itself). Of the claim-counter pools only altw stage 2 blocks on another
  thread, on its shared slot-bucket writers (`s2_slot_flush_lock_wait_ns`);
  stage 3 and pass 1.5 say at the pool why they have no counter. Every
  such wait goes through the gated helpers in `crates/lib/src/counters.rs`, which
  touch no clock unless the `try_*` attempt fails. All unmeasured; each
  step is its own commit.
- **The last silent pools (2026-10-26).** Two kinds of pool still recorded
  no wait. The merge engine behind `sort` and `cat --dedupe` streamed its
  overlap runs through a hand-driven protocol beside the ordered engine (a
  dispatcher thread at the shared admission gate, a `Mutex<Receiver>` job
  queue, an order channel). It now runs on the engine's streamed entry
  point, `run_ordered_streams`, where the consumer issues each task with its
  own channel; its blocked sends and receives go to
  `ordered_pool_result_send_wait_ns` and `_recv_wait_ns` (the write loop
  waiting on a sweep is the receive), and its workers' wait for a job to
  two counters of its own, `ordered_pool_job_lock_wait_ns` and
  `_job_recv_wait_ns`, since a worker with no task is not parked at an
  admission gate. A Denmark `cat --dedupe` of a file with itself (forced
  run on bygg, not stored) showed the job lock wait at about the receive
  wait times the idle peers, as the holder keeps the lock across its
  receive. Extract simple's node and way classify phases were two
  hand-built unordered pools; they now run on `run_unordered_per_blob`
  (through `parallel_scan_blobs_raw`, whose work now gets the schedule
  position) and record the `classify_unordered_*` set, and in exchange gain
  the dispatcher's sliding `WILLNEED` advice the other classify pools have.
  The advice was later measured at HEAD as neither a benefit nor a cost
  (`reference/performance/classify.md`, "WILLNEED on simple's node and way
  classify"); the rest of the move stays unmeasured in isolation.

