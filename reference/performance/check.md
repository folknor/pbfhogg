# Check

## check --ids --full rereads only blobs that may share an ID (bygg, 2026-11-26, against `99c4743c`)

The proof pass now keeps each blob's canonical ID interval and whether it
is in strict order. A kind out of order has its duplicate pass read only
the blobs in overlapping interval components and the blobs out of order
inside themselves; a strict blob whose interval meets no other is skipped.
Retained violation details are also capped at the detail limit per blob
and per kind. `brokkr bench --iterations 1 id-order`, interleaved, the
variant uncommitted (dirty rows), base by `--commit HEAD`.

`--dataset denmark --snapshot unsorted --full yes` (every kind out of
order; `degrade --unsort` overlaps one adjacent blob pair per kind):

| Arm | UUID | Wall |
|---|---|---:|
| base | `438c6c94` | 418 ms (first after build, cold) |
| variant | `59f5baef` | 111 ms |
| base | `68651343` | 238 ms |
| variant | `87c82986` | 112 ms |
| variant | `89bfb91b` | 107 ms |
| base | `fdc09a07` | 234 ms |

-53 %: the duplicate pass (`VERIFYIDS_DUPLICATES`) takes 6 ms where it
reread every blob of all three kinds.

`--dataset germany`, plain `check --ids`, sorted, the cost of the interval
bookkeeping on ordered input (final variant, the out-of-order path out of
line): base `2daf671e` 879 ms, `8ce05b85` 888 ms; variant `d73d70b6` 888
ms, `ca0696f5` 899 ms, `46f820df` 902 ms. +1.6 % on medians, noise. A first
variant with the bookkeeping inline in the element loop measured 922 to
936 ms against 883 to 892 ms (+4 %, every pair the same sign), which is
why the out-of-order path is a cold function.

## check --locations skips ways a partition does not own (bygg, 2026-11-26, against `3789a66a`)

Input: planet `altw-keep-untagged` (10,447,738,627 node records,
1,165,589,744 ways, 12,435,459,911 refs), 6 partitions on this host.
Every partition after the first decompresses every way blob; the variant
pre-scans each way's refs and skips the way whole (no coordinate decode,
no events) when none falls in the partition. The first partition still
reads every way unfiltered and makes every structural check.
`brokkr bench --iterations 1 location-integrity --dataset planet`,
interleaved, variants uncommitted (dirty rows):

| Arm | UUID | Wall | Partitions total |
|---|---|---:|---:|
| base | `fc544bcf` | 175.0 s | 151.2 s |
| blob masks | `caf54bff` | 175.9 s | - |
| per-way skip | `8bb3d5b5` | 162.6 s | - |
| base | `95bbb36c` | 175.4 s | - |
| per-way skip | `291166a7` | 163.8 s | 140.5 s |

Per-way skip: 163.2 s against 175.2 s, -6.8 %, both pairs the same sign;
the node pass (16.6 s, 16.7 s) is unchanged. Landed.

Blob masks (the first partition records which partitions each way blob's
refs touch, later partitions read only the blobs touching them): 171,300
blob decodes against 171,301, one blob skipped planet-wide, because at
blob granularity every way blob touches every partition. Refused.

Routing way-reference observations into node-ID buckets on scratch,
decoding each way blob once, was refused on arithmetic, not measured:
12.4 billion refs at 16 to 24 bytes is 200 to 300 GB written and read
back, at a few GB/s as much time as the whole current run, on a check whose
contract uses no scratch. What remains of the rereads (each later
partition still decompresses every way blob and scans every ref) needs
fewer partitions: a lossless compressed coordinate store, or a node-side
lookup that leaves the way blobs read once.

## Reserved ID-set chunks materialise on first write (bygg, 2026-11-25, against `186acde8`)

`IdSet::pre_allocate` now reserves its direct chunks and an atomic setter
allocates each 4 MiB bit array on first write, instead of allocating the
whole floor range (about 2 GB for the node, way and relation floors) up
front. Planet, where every chunk is written, has to come out neutral:
`brokkr bench --iterations 1 ref-integrity --dataset planet`, interleaved,
base `--commit 186acde8` against the uncommitted variant (dirty rows):

| Arm | UUID | Wall |
|---|---|---:|
| base | `568c35ed` | 24.7 s (first after the build, cold) |
| variant | `6d868df2` | 20.6 s |
| base | `ec0fc3f3` | 19.9 s |
| variant | `04a8a61b` | 20.3 s |
| variant | `7b3d6bf0` | 20.3 s |
| base | `b5cd1500` | 20.0 s |

Warm cells: 20.3 s against 20.0 s, under 2 %, neutral. The win is on
small inputs: twelve concurrent pair-attribution test bodies in one
process went from 7.4 s to 0.19 s.

## A bare check reads an unindexed file once (bygg, 2026-11-25, against `0dccb796`)

Before: on an unindexed file `check::pair` ran the ID check's engine and
then the reference check's, each walking the header chain and reading every
blob (the pair's own walk discarded). After: one walk and one
`parallel_project_phase` over every blob, one projection for both checks;
the reference check's re-reads of dropped judgements and the `--full`
duplicate pass follow as before.

`brokkr bench --iterations 1 default-integrity --dataset planet --variant raw`
(`planet-20260223.osm.pbf`), interleaved, variant measured uncommitted on
`0dccb796` (stored as dirty rows):

| Arm | UUID | Wall |
|---|---|---:|
| base `0dccb796` | `555d1e7d` | 41.5 s |
| fused | `e4fa4059` | 21.1 s |
| base `0dccb796` | `1d50bb2d` | 36.8 s |
| fused | `85686a66` | 21.1 s |

Sidecar `85686a66`: `CHECKPAIR_UNINDEXED` 21,045 ms; the way re-read
phase and deferred resolve under 1 ms each (no blob re-read).

The node replay now skips IDs of other kinds (the pass's ID scratch may
hold relation IDs), a branch the indexed node phases also take. Indexed
default pair, same commit, interleaved: base `0e379bb2` 28.1 s (first
after the unindexed cells) and `4670a74b` 21.0 s; variant `4c3d3db7`
21.9 s and `6a27f41f` 21.2 s. Neutral.

## A bare check reads an indexed file once (bygg, 2026-11-24, against `3c729629`)

`check` with no selector (`--ids` with `--refs`) used to be two library
calls, each walking the header chain and reading every node and way blob.
It is now one call (`check::pair`) that walks once and reads each kind's
blobs once for both checks. Planet indexed, `default-integrity --dataset
planet`, interleaved `--iterations 1`, base `3c729629` (`--commit`)
against the working tree on it (dirty, stored as such):

| Arm | Wall | UUID |
|---|---:|---|
| base | 44.1 s | `f06ab0cd` |
| pair | 21.1 s | `d2ad34b3` |
| base | 37.0 s | `a8f2a0c5` |
| pair | 21.2 s | `9a25b81d` |

About -45 %. The phase split (`a8f2a0c5` against `9a25b81d`): the base
ran `VERIFYIDS_NODES` 9.1 s, `VERIFYIDS_WAYS` 7.7 s, `CHECKREFS_NODES`
9.3 s and `CHECKREFS_WAYS` 10.5 s; the pair ran `CHECKPAIR_NODES` 10.2 s
and `CHECKPAIR_WAYS` 10.7 s, each at about 30 cores, about 57 GB and 24 GB
read once instead of twice. Peak anon about 2.16 GB in both, the reference
check's node set. The standalone `check --refs`, whose code was only
refactored, is unchanged: `ref-integrity` 19.9 s base (`12461e5e`), 20.1 s
working tree (`058daf33`).

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

`--full` now runs the plain order scan first (which, until the 2026-11-18
section "check handles negative IDs exactly", recorded negative IDs as `IdOutOfRange`); a kind in strict `osm_id_cmp` order cannot hold
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

## check handles negative IDs exactly (bygg, 2026-11-18, against `f1fc6f76`)

The ID check's per-element judge gains one sign test (`has_negative_ids`)
and `--full` drops its out-of-range report: a negative now spills to the
duplicate pass's side set as an ID above the allocation does. `--refs` and
`--locations` lose refusal branches only. The cell is the one the change
reaches on positive input: `id-order --full yes`, denmark `--snapshot
unsorted` (every kind out of order, so the duplicate pass runs),
interleaved `--iterations 1` A B B A, base `f1fc6f76` (`--commit`) against
the uncommitted working tree:

| Arm | Walls |
|---|---:|
| base | 369 ms (`629e2a61`, first after the build, cold), 244 ms (`6e7ec2c0`) |
| working tree | 241 ms (`f3dc6755`), 239 ms (`cd2d04a7`) |

Against the warm base cell the working tree is within noise (-1 to -2 %):
no cost. No dataset holds a negative ID, so no cell measures one.

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

## History

### Check commands

#### check --refs (commits `8f0ccbb`, `053def6`, `fbf591c`, 2026-04-17)

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

#### check --ids --full (commits `855b3b2`, `0d71b3b`, 2026-04-17)

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

