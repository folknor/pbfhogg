# Geocode

## build-geocode-index

Reverse geocoding index build. 4-pass pipeline: nodes (address points + dense
node index), ways (streets, buildings, interpolation), relations (admin boundary
assembly + simplification), S2 cell assignment (fine level 17 + coarse level 14).

| Dataset | PBF size | Time | Index size | Addr points | Streets | Admin | Commit |
|---------|----------|------|------------|-------------|---------|-------|--------|
| Denmark | 465 MB | **7.1s** | 172 MB | 2.6M | 314K | 2K | `f42da6e` |
| Japan | 2.4 GB | **26.7s** | - | - | - | - | `c33e8cc` |
| Germany | 4.5 GB | **1813s** (30m) | ~1.8 GB | 19.8M | 3.3M | 43K | `ed34092` |

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

