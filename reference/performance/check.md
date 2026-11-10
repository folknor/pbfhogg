# Check

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

