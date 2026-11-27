# Geocode

## Format 10: persisted interpolation prefixes (bygg, 2026-11-26, `1361533e` against `da1f1a55`)

Format 10 stores each interpolation section's total and segment prefix
lengths and drops unresolved ways; Pass 1.5 spills fold into one shared
set; Pass 2 classifies address keys once per string entry. Germany,
`--iterations 1` interleaved:

| Bench | `1361533e` | `da1f1a55` |
|---|---|---|
| `geocode-build` | 7.5 s `db0e2f48` (cold), 6.2 s `bf1d66e6` | 6.1 s `fcd069b0`, 6.2 s `be3ce6c5` |
| `geocode-query`, measured call | 12.8 s `a1361a74`, 12.8 s `2f2c19f9` | 11.2 s `047c39f7`, 11.3 s `455be5b9` |

Build neutral; query about 12 % faster. The index has 2,417 interpolation
records instead of 2,724 (307 unresolved ways no longer written).

The CLI's parallel batches, through `geocode-query-cli` (the user-facing
command over the library bench's point sequence), germany measured call:
`1361533e` 3.9 s `ba89b165`, 3.9 s `16ec1d27`; `6808a7c1` 1.2 s
`a86fd3b0`, 1.1 s `cb3f3e24`. About 3.4x faster.

## build-geocode-index

Reverse geocoding index build. 4-pass pipeline: nodes (address points + dense
node index), ways (streets, buildings, interpolation), relations (admin boundary
assembly + simplification), S2 cell assignment (fine level 17 + coarse level 14).

| Dataset | PBF size | Time | Index size | Addr points | Streets | Admin | Commit |
|---------|----------|------|------------|-------------|---------|-------|--------|
| Denmark | 465 MB | **7.1s** | 172 MB | 2.6M | 314K | 2K | `f42da6e` |
| Japan | 2.4 GB | **26.7s** | - | - | - | - | `c33e8cc` |
| Germany | 4.5 GB | **1813s** (30m) | ~1.8 GB | 19.8M | 3.3M | 43K | `ed34092` |

### Pass 3 stage B encodes and writes cell ranges in parallel (bygg, `b4e65632`)

Fine and coarse buckets split at cell boundaries. Workers encode the ranges
in parallel, then write to disjoint ranges assigned by prefix offsets. The
serial writer remains under `cfg(test)` as a byte oracle.

Germany `geocode-build` on bygg, with `--iterations 1` interleaved. The
rows are in run order.

| Arm on bygg | Wall | Run | Note |
|---|---:|---|---|
| A, HEAD `b4e65632` | 7.7 s | `e58f5eec` | Cold first cell, discarded |
| B, working tree on `b4e65632` | 6.1 s | `6215952c` | |
| A, HEAD `b4e65632` | 6.5 s | `9cd20424` | |
| B, working tree on `b4e65632` | 6.1 s | `b51e4079` | |

On bygg, B cut wall time by about 6 % versus the warm A at `b4e65632`.
The largest bucket still sorts on one core, and pass 2 still has a single
consumer. Germany `verify geocode` and `verify geocode-seam` passed on
bygg with the working tree on `b4e65632`.

### Prepared admin rings in Pass 3 (bygg, 2026-11-27)

Pass 3 prepares each polygon's rings once, uses the padded edge cover to
classify boundary cells, and reuses one center test per edge-free subtree.
The legacy flood fill remains a test-only oracle.

Germany `geocode-build` on bygg, single-iteration cells interleaved baseline
first:

| `0d1e05fb` | working tree on 0d1e05fb |
|---|---|
| 8.3 s `075b3636` (cold first cell, excluded), 6.8 s `d9cf2233` | 6.6 s `552620d0`, 6.3 s `32abf21d` |

The sidecar comparison of `d9cf2233` (`0d1e05fb`) and `32abf21d` (working
tree on 0d1e05fb) on bygg shows the `GEOCODE_PASS3_ADMIN_CELLS` span falling
from 125 ms to 43 ms (65 % shorter). Most of the remaining wall delta is
in the publish segment, where writeback timing varied. The measured gain
is in admin cell assignment.

Denmark `geocode` verify passed on the working tree on 0d1e05fb on bygg.

### Rings on one line refused, flattened rings stored whole (bygg, 2026-11-24, variant uncommitted against `3b534693`)

Planet, one `--commit HEAD` baseline cell and one variant cell with
`--iterations 1`, baseline first. Baseline `dae479f3` 125.2 s, variant
`6909ca6d` 119.1 s. The gap is mostly in `GEOCODE_PASS3_ADMIN_CELLS`
(13.4 s against 8.4 s, 134 against 83 samples), which the change does
not touch: cache state, not code. The changed phase,
`GEOCODE_PASS2_ADMIN_ASSEMBLY`, ran 2.19 s against 2.18 s. Both cells
dropped 3049 admin relations, so no planet relation has a ring the
collinearity refusal catches. Verdict: no measured cost.

### Hole ownership by one exact witness, start-dependent rings refused (bygg, 2026-11-24, variant uncommitted against `8d9fdaf9`)

Planet, one variant and one `--commit HEAD` baseline cell with
`--iterations 1`, variant first. Variant `fbd32849` 123.7 s, baseline
`532145fb` 118.2 s. The whole gap is in Pass 2a (26.5 s against 20.1 s,
96 GB against 64 GB read), a phase the change does not touch: page cache
state, not code. The changed phase, `GEOCODE_PASS2_ADMIN_ASSEMBLY`, ran
2.19 s against 2.16 s. Both cells dropped 3049 admin relations, so no
planet relation has a ring the new start-invariance refusal catches.
Verdict: no measured cost.

### Pass 2a returns the first node error (plantasjen, 2026-11-20, variant uncommitted against `e2387c31`)

Germany, one baseline and one variant cell with `--iterations 1` and
`--stop GEOCODE_PASS2A_END`. Trim was unavailable in the sandbox. The
`--commit HEAD` attempt (`005e5a6b`) stopped before measurement because
Git could not create a worktree, so the baseline ran from the working
tree before the production code edit. Baseline `c1bd30f7` ran in 5.90 s
with 1.86 s in Pass 2a. Variant `36218575` ran in 4.00 s with 1.72 s
in Pass 2a. The schedule and Pass 1.5 phases read less from disk in the
second cell. Verdict: no measured cost in Pass 2a. The pair's cache
conditions differ, so it gives no speedup estimate.

### Admin way geometry in two flat mappings (plantasjen, 2026-11-20, variant uncommitted against `8766940a`)

Pass 2b staged every admin way as two heap vectors, node IDs and
coordinates; the variant copies the way's packed delta refs and its
coordinates into two anonymous mappings sized once from Pass 1.5's
admin totals, and assembly decodes the refs per relation. Planet, full
build, `--iterations 1`, one pair, no trim: variant `61256a0c` (after a
long unrelated corpus run), base `f380b1bb` (`--commit HEAD`, right
after). Walls 210 s and 203 s. Peak anon in Pass 2b 13.6 GB against
14.2 GB. From admin assembly to the end of the run the base holds 4.6
to 5.0 GB anon and the variant 2.4 to 3.2 GB: the heap kept the freed
per-way vectors resident, and the mappings return to the OS on drop.
Pass 3 stage B fine peaks at 8.8 GB anon against 11.6 GB, the smoke test
at 3.0 GB against 4.8 GB. Output identical: 929,216 admin polygons,
3,049 relations dropped, 324 topology refusals. Verdict: kept; the wall
delta is inside the band of one unbalanced pair.

Bygg, 2026-11-20, committed as `3db3087d` against its parent
`8766940a`, one pair, no trim: variant `3ed58022` 118 s, base
`e2f56dbc` 129 s (`--commit`, second). Anon from admin assembly to the
end 2.6 to 3.0 GB against 4.8 to 5.4 GB, stage B fine 9.7 GB against
12.8 GB, smoke test 2.7 GB against 5.3 GB, Pass 2b 13.9 GB against
14.2 GB. The pair also settles the Pass 2a question left by the
node-topology entry below: Pass 2a's code is the same in both arms, yet
the base read 100 GB in 27.7 s and the variant 72 GB in 23.0 s. Pass
2a's read volume follows page-cache state, not code, so the 61.9 GB
against 87.8 GB there was not the ring rewrite's.

### Admin rings by node topology (bygg, 2026-11-19, variant uncommitted against `9d61febc`)

Admin ring assembly replaced: members join by node ID and each graph is
cut into biconnected blocks (a cycle block is a ring, any other block or
a self-touching, degenerate or nested ring refuses the relation), in place
of the budgeted endpoint-coordinate search. Pass 2b keeps every admin
way's node IDs. Planet, full build, `--iterations 1`, after
`sudo fstrim -av`, baseline built with `--commit HEAD`. Walls: base 114 s
(`98348426`), variant 123 s (`a5b7acf7`, first cut with std hashing),
variant 122 s (`deac9c4c`, Fx hashing, closed-member fast path), base
113 s (`fc33a797`). Second pair by phase: `GEOCODE_PASS2_ADMIN_ASSEMBLY`
1.17 s to 2.29 s, `GEOCODE_PASS2B` 6.45 s to 7.97 s, peak RSS after
Pass 2 about 2 GB higher. `GEOCODE_PASS2A`, whose code is unchanged,
ran 19.3 s in both base cells and 25.4 s in both variant cells, reading
87.8 GB instead of 61.9 GB from disk; not explained by the change and not
resolved by these cells. Output: 929,216 admin polygons instead of
929,941, 3,049 relations dropped instead of 2,742, of which 324 are
topology refusals (the old search refused 3 on budget). Verdict: costs
more than noise and drops 307 more relations; held uncommitted for the
maintainer's decision.

### Building points take the short longitude arc (bygg, 2026-11-25, variant uncommitted against `236e4ddc`)

Pass 2b places a building at the raw node mean when its longitude span is
under half a turn, and otherwise averages in a frame lifted by a full turn
or gives no point; the change adds one span scan per building way. Planet,
full build, `--iterations 1`, interleaved A B A B, baseline built with
`--commit HEAD`, no trim. Walls: base 113 s (`52687338`), variant 111 s
(`c0a4990e`), base 116 s (`f775a489`), variant 115 s (`1b343e78`).
`GEOCODE_PASS2B` in the second pair: base 5.43 s, variant 5.65 s, inside
noise. Verdict: neutral.

### Missing way nodes never join their neighbours (bygg, 2026-11-19, variant uncommitted against `80774411`)

Pass 2b resolves each way's refs in one loop that records where a run of
resolved refs breaks; streets emit one record per run, an incomplete
building gives no address point, and an incomplete admin way withholds
its relations. Planet, full build, `--iterations 1`, interleaved A B A B
after `sudo fstrim -av`, baseline built with `--commit HEAD`. Walls: base
112.6 s (`752c6de4`), variant 113.1 s (`86946794`), base 111.5 s
(`6b5c0402`), variant 111.6 s (`329d2309`). `GEOCODE_PASS2B` in the second
pair: base 5.28 s, variant 5.10 s. Planet has no unresolved way ref, so the
counters (admin relation drops 2742, missing member geometry 0) and the
output are unchanged. Verdict: neutral.

### Pass 2a judges the domain on exact nanodegrees (bygg, 2026-11-19, variant uncommitted against `364e45e6`)

Pass 2a computes every node's exact nanodegree position in `i128` and
checks the geographic domain on it before truncating, instead of judging
the wrapped and narrowed `decimicro_lat`. Planet, full build,
`--iterations 1`, interleaved A B A B after `sudo fstrim -av`, baseline
built with `--commit HEAD`. Walls: base 107.7 s (`c98a41fc`), variant
115.8 s (`410578d9`), base 116.6 s (`047778be`), variant 116.7 s
(`b48cb68d`); the first cell's lead is `GEOCODE_SCHEDULES` (0.6 s against
6.5 to 7.1 s in the other three), the header walk this change does not
touch. `GEOCODE_PASS2A`: base 20.39 s, 19.24 s; variant 19.45 s, 19.51 s.
Verdict: neutral.

### Negative node refs spill to the overflow lists (bygg, 2026-11-18, variant uncommitted against `6a823de7`)

Pass 1.5 collects negative admin ways and node refs: a ref the
pre-allocated range does not cover (negative, or above it) goes through
`try_insert_atomic` to the worker's overflow list, replacing the sign skip
and its tally. Germany, `--stop GEOCODE_PASS1_5_END`, `--iterations 1`
in A B B A order, no fstrim (denied). GEOCODE_PASS1_5_SCAN: base 717 ms
(`f7d6e98a`), variant 705 ms (`96e0dc7a`), variant 703 ms (`b8904b54`),
base 718 ms (`ed7205d0`). Walls are 901 ms in all four (the stop lands at
a fixed point after the marker). Verdict: neutral, about 2 % faster, within
noise.

### Input digest in the header, format 7 (bygg, 2026-11-15, variant uncommitted against `9609d308`)

The build hashes the whole input (XXH3-128, a full sequential read of the
88 GB planet file) on its own description beside the passes. Planet,
interleaved `--iterations 1`, baseline `--commit 9609d308`, variant
uncommitted (dirty rows).

First shape, joined after the Pass 2 scan: base 118.4 s (`b9ed7652`),
variant 115.6 s (`e84ffb7f`), base 123.5 s (`e3f3a4da`), variant 138.2 s
(`f37d7dd6`). The join waited 5.0 s and 9.5 s
(GEOCODE_INPUT_DIGEST_JOIN): the hash does not finish inside the reading
passes (about 37 s), and Pass 1.5 ran 0.6 to 1.7 s slower. Rejected.

Landed shape, joined only for the header write, so Pass 3 (which reads no
input) hides the rest: variant 112.9 s (`b21f86d6`), base 119.5 s
(`08ed6542`), variant 126.4 s (`2506075f`). The join waited 0 ms in both
variant cells. Through the end of the Pass 2 scan the variant is 44.8 and
45.8 s against 43.4 to 44.0 s in the four baselines, about 1 to 2 s of
read contention; Pass 3 swings by 10 s in both arms on identical code.
Verdict: within noise on the wall; the digest costs no measurable time at
planet.

### G1: way blobs decompressed once, Pass 2b off the PBF (bygg, 2026-11-10, `9d9a2a79` against `0b52650b`)

Interleaved `--iterations 1` pairs after a trim, both arms `--commit`.
Denmark: 1.3 s (`ab82dd55`) against 1.2 s (`72227dc8`), 1.2 s
(`f946f8e2`) against 1.2 s (`d3aaae61`); too small to resolve. Germany:
11.6 s (`32280d21`) against 11.1 s (`2cb622e8`), 10.7 s (`0d670319`)
against 10.9 s (`089830dc`); no consistent sign on the wall.

The phase split is not neutral. GEOCODE_PASS2B, the span G1 was meant
to shrink, got slower in both pairs (2,617 to 2,848 ms in pair 2, +20 %
in pair 1), and its core count fell from 11.2 to 3.9 in pair 1: the
ordered pool over the way store runs far less parallel than the old
decode. GEOCODE_PASS1_5_SCAN, now writing the store, is +18 %. Pass 3
and admin assembly are a little faster, and RSS after Pass 2 drops from
about 2.0 GB to 0.4 GB. Verdict: not a win as landed; Pass 2b's lost
parallelism is the lead. No planet cell run until that is understood.

### Pass 2b's merge fed packed worker output (bygg, 2026-11-10, `4b08aa07` against `9d9a2a79` and `0b52650b`)

Pass 2b's single merge thread was the bottleneck before and after G1:
workers spent 50.0 s, then 82.7 s, blocked handing results over inside
a phase under 3 s, while the merge waited 8 ms, then 4 ms, for work.
`4b08aa07` packs each blob's output on the worker (one text buffer with
precomputed pool hashes, street coordinates pre-encoded into one byte
buffer written in one call per blob) and drops Pass 1.5's per-value
key matching.

Germany, `--iterations 1`, after a trim, three arms all `--commit`, in
the order A B C, C B A, A B C:

| Arm | Cells |
|---|---|
| A, pre-G1 `0b52650b` | 11.7, 10.6, 10.7 s (`a1f4ad69`, `b2164581`, `db7fd364`) |
| B, G1 `9d9a2a79` | 11.3, 11.2, 11.0 s (`8d1a4170`, `e8ed3ec3`, `fac59171`) |
| C, packed merge `4b08aa07` | 8.8, 8.8, 8.8 s (`339975a6`, `2351720c`, `de1fb6a5`) |

C is about 17 % under A on the wall, every cell. Phase by phase against
A (`db7fd364` against `de1fb6a5`): GEOCODE_PASS2B 2,561 to 594 ms;
GEOCODE_PASS1_5_SCAN 682 to 796 ms, the cost of writing the way store
and the validation G1 moved there; RSS from Pass 2 admin assembly on
about 2.0 GB to 0.35 GB. Verdict: G1 plus the packed merge kept. The
germany index built at `4b08aa07` is byte-identical to `0b52650b`'s, all
19 files by md5, including the 2,724 interpolation records denmark
lacks.

Planet (`planet-20260223-reindexed`), `--iterations 1`, both arms
`--commit`, order A C, C A: A 181 s (`397200b0`) and 177 s
(`96ce7e1a`); C 155 s (`4a802fab`) and 154 s (`9228b533`), about 13 %
faster. Against `96ce7e1a`, C's GEOCODE_PASS2B goes from 33.3 to 7.9 s
and its disk read from 26.1 to 6.5 GB; GEOCODE_PASS1_5_SCAN rises from
8.9 to 11.1 s; peak RSS in Pass 2b falls from 13.7 to 12.8 GB. The way
store is 7.2 GB of scratch (`geocode_way_store_bytes`).

### Pass 1 reads only the relation blobs (bygg, 2026-11-10, `da55488a` against `4b08aa07`)

Pass 1 read the whole input to reach the relation blobs, since the
sequential reader's relation filter skips decode, not the read. It now
decodes only the relation schedule from the header walk, in parallel.

After a trim, `--iterations 1`, both arms `--commit`. Planet, order A B,
B A: A 154 s (`6ddd7015`) and 153 s (`fc598497`), B 127 s (`e83f931b`)
and 124 s (`4dc30ff2`), about 18 % faster. Against `fc598497`,
GEOCODE_PASS1 goes from 28.3 s at 0.4 cores reading 87.5 GB to 0.37 s
reading 85 MB; every other phase is within a few percent. Germany,
same order: A 9.4 s (`fa053349`) and 8.8 s (`4e0f2e90`), B 6.9 s
(`8c334b91`) and 6.9 s (`f162c983`). Germany gains more than its 0.58 s
Pass 1: against `4e0f2e90`, GEOCODE_SCHEDULES also falls from 1,265 ms
reading 2.6 GB to 31 ms, as the walk no longer runs after a full
sequential read of the file. The germany index is byte-identical
across the change, all 19 files by md5.

### Admin rings simplified to 20 m instead of a vertex cap (bygg, 2026-11-10, `9470143c` against `8094648a`)

After a trim, `--iterations 1`, both arms `--commit`. Germany A B B A:
6.7 s (`924fd3d9`) and 6.5 s (`995dca50`) against 6.3 s (`e4dae083`)
and 6.3 s (`632417dd`). Planet one pair: 117 s (`0a083f67`) against
109 s (`05725781`). Germany index built once per arm: admin_vertices.bin
103.4 MB against 41.7 MB, so the 20 m tolerance keeps fewer vertices
than the old level-scaled cap, not more; admin polygons 43,091 against
43,044 and dropped relations 1,059 against 1,060, the polygons that fit
inside the tolerance; every non-admin file unchanged in size. The
default is not slower to build and stores less admin geometry.

Query latency, geocode-query, 100k seeded points in the Germany box
5.9,47.3,15.0,55.0, best of 3 rounds, child `us_per_query_best`. The
vertex-cap arm is `74df55c6`, the bench commit's CLI on `8094648a`'s
library (a commit object outside main), against `9a7e719b`. Germany
A B B A: 91.06 (`91156a4f`) and 98.72 us (`4018afe3`) against 83.98
(`05b87e31`) and 86.52 us (`53a67bb6`), about 10 % faster. Planet one
pair: 127.21 us (`acaf50e0`) against 108.39 us (`74b3c642`), about 15 %
faster. Answers differ as expected: germany points with an admin answer
70,254 against 70,246; planet admin polygons 929,941 against 910,045,
about 20,000 fewer, with dropped relations 2,742 against 2,753. These
were not only polygons inside 20 m: rings collapsed to stored zero-area
lines and holes collapsed with them, so exclaves answered as their
neighbour. The commit after `9e098409` reruns a collapsed ring at a
floor of its own width; those cells are recorded below it.

### Collapsed admin rings rerun at their own width (bygg, 2026-11-10, `1d982110` against `9e098409`)

geocode-query, same box and seed as above, both arms `--commit`, A B B
A on germany: measured call 34.2 s (`086e777b`) and 33.9 s
(`96a355f3`) against 34.1 s (`53c57d19`) and 34.3 s (`e3b0c042`),
neutral. Planet one pair: 105.51 us per query (`3bf32539`) against
106.95 us (`f385f357`), neutral, with the same admin digest on the
query box. The planet build restores 929,941 admin polygons and 2,742
dropped relations, the vertex cap's counts, against 910,045 and 2,753.

### Pass 3 bucket writes in parallel, stage B on the ordered pool (bygg, 2026-11-10, `b9ccca65` against `9775b0e2`)

Stage A groups each chunk's cover results by bucket and writes bucket
files in parallel; stage B runs on `run_ordered` over the 256 buckets,
window 8, each task sorting and encoding its bucket.

After a trim, `--iterations 1`, both arms `--commit`, A B B A. Planet:
A 123 s (`b057fb63`) and 122 s (`7e604c68`), B 109 s (`67b764ef`) and
110 s (`4236420d`), about 10 % faster. Against `7e604c68`:
GEOCODE_PASS3_STAGEA_STREETS 17.0 to 10.4 s, STAGEA_ADDR 4.3 to 2.6 s,
STAGEB_FINE 21.0 to 18.1 s, STAGEB_COARSE 9.6 to 8.1 s; stage B peak
anon 10.6 to 11.0 GB. Stage B still averages only 2 cores: one heavy
bucket's single-threaded sort is the floor, as the design predicted.
Germany pair: 6.8 s (`e119064b`) against 6.4 s (`7ff304fc`). The
germany index is byte-identical across the change, all 19 files by
md5. Kept.

### Advice off the merge thread, 2b lookups on workers (bygg, 2026-11-10, `9775b0e2` against `da55488a`)

Read-ahead advice in the ordered classify pool moves to its own thread,
Phase 2a's output takes 2b's packed form, and 2b's workers look strings
up in a read-only snapshot of the pool so the merge interns only misses.

After a trim, `--iterations 1`, both arms `--commit`, A B then B A.
Planet geocode-build: A 127 s (`974180ac`) and 126 s (`d2cc07db`), B
123 s (`c8aeda0b`) and 123 s (`63ddabda`), about 3 % faster. Against
`d2cc07db`, GEOCODE_PASS2A 21.9 to 19.0 s and GEOCODE_PASS2B 8.2 to
7.2 s; every other phase unchanged. The germany index is byte-identical
across the change, all 19 files by md5. Guard on the other ordered
classify users, planet `copy-pbf --clean yes`: A 142 s (`3be27943`) and
142 s (`eddd7768`), B 143 s (`67d379e5`) and 143 s (`39ddb8ac`),
neutral. Kept.

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
difference is the segment after the admin index (about 14 to 16.5 s,
24.8 to 26.0 GB written). That segment held the header write and the
smoke test's reader-open validation, and these cells do not separate the
two, so what in it the larger records cost is unattributed. Builds since `GEOCODE_SMOKE_TEST` was added time the smoke
test as its own span and end `GEOCODE_PASS3` before the header write.
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

`resolve_interpolation_endpoints_mmap` (`crates/lib/src/geocode_index/builder/interp.rs`)
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

### Whole answers black-boxed, query scopes under hotpath (bygg, 2026-11-25, variant uncommitted against `eb7ba68c`)

The instrument passes each whole `ReverseResult` to `black_box` instead
of summing admin counts, so fat LTO cannot drop the address, street and
interpolation assembly; `Reader::query` and its level, admin and
interpolation searches gained `hotpath` scopes, compiled out of timing
builds (registry version 6). geocode-query, germany, country bbox
`5.9,47.3,15.0,55.0`, 100k points, 3 rounds, two interleaved
`--iterations 1` pairs (base built with `--commit HEAD`):

| Cell | us per query (best) | rounds | open_ms |
|---|---:|---|---:|
| base `d8516de4` | 18.71 | 18.71, 18.71, 18.77 | 915 |
| variant `5858f1f6` | 19.16 | 20.61, 19.98, 19.16 | 869 |
| base `50c8ae66` | 18.69 | 18.72, 18.69, 18.86 | 918 |
| variant `45f9aab2` | 18.84 | 20.63, 19.58, 18.84 | 880 |

About 1.6 percent slower on the best round, under the noise band and not
chased; that is no evidence the old sink let work be eliminated, only that
it is no longer possible. The variant's first timed round ran slower in
both pairs and the bases' did not; the best round is the figure compared.

### Segment projection framed by the integer longitude delta (bygg, 2026-11-24, variant uncommitted against `7f2024af`)

The reader frames each street and interpolation segment from the integer
longitude delta and projects onto both half-world paths only at an exact
half turn; the builder's ranking area changes only for rings across the
antimeridian. geocode-query, germany, country bbox `5.9,47.3,15.0,55.0`,
100k points, 3 rounds, one `--iterations 1` pair (base built with
`--commit HEAD`):

| Cell | us per query (best) | open_ms |
|---|---:|---:|
| base `d3884255` | 19.04 | 849 |
| variant `65cd7e28` | 18.46 | 851 |

Not slower; the 3 percent in the variant's favour matches the sign every
`--commit HEAD` base in this file shows against a dirty variant and is not
chased.

### Distance hits rank by a total order, inclusive winner bounds (bygg, 2026-11-24, variant uncommitted against `b7626a0c`)

`query()` ranks address, street and interpolation hits by squared distance
then record identity, and the search walk admits a cell whose lower bound
equals the current winner's distance, so an exact tie can still be met.
geocode-query, germany, country bbox `5.9,47.3,15.0,55.0`, 100k points, 3
rounds, interleaved `--iterations 1` cells (base built with `--commit HEAD`):

| Cell | us per query (best) | open_ms |
|---|---:|---:|
| base `5707f48c` | 19.50 | 857 |
| variant `0d83fa69` | 18.44 | 830 |
| base `cf2dbfcf` | 19.07 | 865 |
| variant `f7c9b2c4` | 18.40 | 855 |

The variant is not slower; the 3 to 5 percent in its favour has the same
sign in both pairs but sits in the band not chased. The inclusive bound
differs from the strict one only on exact float equality, so it reads no
extra cells on this workload.

### Admin lookup reads the own cell and trusts its flag, format 9 (plantasjen, 2026-11-20, variant uncommitted against `a7946048`)

The reader reads only the query's own admin cell instead of nine and accepts
an interior-flagged entry without a point-in-polygon test (ADR-0029).
geocode-query, germany, country bbox `5.9,47.3,15.0,55.0`, 100k points, 3
rounds, interleaved `--iterations 1` cells (base, variant, base, variant):

| Cell | us per query (best) | open_ms | index build (child 0) |
|---|---:|---:|---:|
| base `15de9fd0` | 131.82 | 1120 | 13.3 s |
| variant `caa66d3a` | 26.48 | 1111 | 12.4 s |
| base `5eed0a30` | 127.92 | 1127 | 13.4 s |
| variant `17b4381f` | 26.08 | 1072 | 13.3 s |

About 4.9 times faster in both pairs. The bench discards answers; the
answer evidence is `pbfhogg-verify geocode --dataset germany` passing on
the variant. The build adds one frame pass per admin polygon (the bound
check) and builds the same cover; its wall moved within noise. What is
left of the query cost is the containment test on unflagged entries, which
still rebuilds the polygon's rings. The variant measured here dropped
unbounded polygons in pass 3; the landed shape drops them after ring
assembly, the same work moved earlier.

### Corrupt entries refused at the read (plantasjen, 2026-11-20, variant uncommitted against `3db3087d`)

Lookups now fail on a cell entry naming a record past its file instead of
skipping it, sinks take the record the search already resolved instead of
re-reading it, and open additionally checks admin cell order, polygon area
and level, a nonempty list over an empty target file, and the empty-string
sentinel. geocode-query, germany, country bbox `5.9,47.3,15.0,55.0`, 100k
points, 3 rounds, interleaved `--iterations 1` cells (base, variant, base,
variant):

| Cell | us per query (best) | open_ms |
|---|---:|---:|
| base `8e049cf3` | 129.31 | 1088 |
| variant `dfbb0430` | 122.33 | 1030 |
| base `ee856753` | 129.05 | 987 |
| variant `64afe2f4` | 122.28 | 1009 |

Queries about 5 % faster in both pairs, same sign (the dropped re-reads in
the sinks and assembly are the likely cause; not attributed further). Open
moves within noise: the new checks ride walks open already made.

### Where the walk still costs (bygg, 2026-11-10, at `7dcabbcc`)

geocode-query on the denmark index, 100k points, seed 1. Open North
Sea `5.0,55.5,7.0,57.5`: 0.95 us per query, no answers (`9581539a`,
dirty tree): the presence test rules out every class before any walk,
so open sea does not pay the walk the table below implies. The
sea-heavy country box `8.0,54.5,13.0,57.8`: 20.60 us per query, 69 %
admin and 39 % address answers (`5f6d309e`, committed). The walk costs
only on coastal and sparse points whose answer lies just past the 3x3
block; a face, i, j walk with a vertex cache could recover an estimated
3 to 5 % there and nothing measurable on land, so it was not built.

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

## History

### build-geocode-index

#### Optimization arc (Denmark, plantasjen)

| Commit | Change | Time | Cumulative |
|--------|--------|------|------------|
| `d27f17e` | Baseline (4 scans, sequential for_each) | 21.4s | - |
| `e7a12e6` | 3 scans (reorder: relations first) | 18.5s | -14% |
| `da4d939` | 2 scans (fused node+way, pipelined) | 10.9s | -49% |
| `60df011` | Zero-alloc cover_segment + parallel S2 cells | 10.4s | -51% |
| `398b1a4` | Block-pipelined, skip_metadata, tag-first way classification | 9.7s | -55% |

#### Germany RSS profile (commit `3449db2`, plantasjen, hotpath)

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

