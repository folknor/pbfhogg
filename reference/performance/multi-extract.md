# Multi Extract

## Multi-extract

Simple strategy on sorted input: one run over the PBF for all N regions,
each kind read once when its blobs carry indexdata ID ranges the fused
planner admits (overlapping blobs read together as one component task) and
classified then rewritten otherwise; each
region's blocks are compressed and framed in the pool's tasks and the frames
written to N sync-mode PbfWriters (publication syncs each output after the
final flush, between `PUBLISH_START` and `PUBLISH_END`; the
`writer_sync_all_ns` and `writer_sync_all_calls` counters attribute that
cost). Kinds run nodes, then ways, then relations, each after the previous
kind's membership is frozen. Older rows below predate the per-output sync,
the 256 KiB per-output buffer, the shared region membership (they used one
`IdSet` per region and kind) and the fused pass. 5 disjoint longitude strips
per configured bench.

### Overlap components in the fused pass (bygg, 2026-11-26, against `61e55c62`)

The whole-kind disjointness gate replaced by overlap component tasks (a
component's `(id, region)` union, each blob emitting only to regions it is
read for). Default 5 strips, `--iterations 1`, interleaved, baseline first,
after one fstrim at the head of the suite (shared with the extract.md
cells).

| Form | planet baseline | planet variant | europe baseline | europe variant |
|------|-----------------|----------------|-----------------|----------------|
| admission checked per occurrence in emission | 49.19 s (`f46d422f`), 49.46 s (`bbacd277`), 50.01 s (`2399c7f2`) | 51.00 s (`c04fad88`), 50.94 s (`b3cea44f`), 51.44 s (`d1a2f12d`) | | |
| admission applied once per blob to the component table (landed) | | | 73.13 s (`1bc567c0`), 73.37 s (`3ad18301`) | 73.14 s (`698561c3`), 73.06 s (`62ab00cb`) |

The first form was about 3 % slower on planet, sign consistent in three
pairs, all in the node phase (`MULTI_NODE_FUSED` 28.9 s against 30.1 s,
27.4 cores both), which points at the per-occurrence region call. The
landed form keeps that call as it was and is neutral on Europe, node phase
36.31 s against 36.16 s. It was not rerun on planet: the item had used its
planet cell budget. No indexed history input is registered, so the
history-side saving is unmeasured.

### Per-region raw copy of node blobs (bygg, 2026-11-25, against `5e1e8843`)

A node blob is nominated and verified for a raw copy per region, as each
single extract decides it, instead of only when it lies inside every
region; this is for output parity, not speed. `brokkr bench --iterations
1 region-extract --mode multi --strategy simple --dataset planet` (5
strips), interleaved, baseline `--commit HEAD` at `5e1e8843`, variant
uncommitted on it:

| Arm | Wall | UUID |
|---|---:|---|
| base | 52.0 s | `50b51ec1` |
| variant | 51.6 s | `fdb15e70` |
| base | 51.5 s | `31a58623` |
| variant | 51.2 s | `e9c29ae8` |

Neutral: the phase split of the second pair is within 1 % per phase
(node fused 28.9 s against 28.7 s). No planet node blob lies inside any
strip, so the only added work is one bbox test per region per node blob.

### Per-region node blob admission (bygg, 2026-11-25, against `6a8edb6f`)

Each node blob carries the regions its bbox meets, and its occurrences
are classified for and written to those alone (simple fused and
two-phase, complete and smart node classify). `brokkr bench --iterations 1
region-extract --mode multi --dataset planet` (5 strips), interleaved,
baseline `--commit HEAD` at `6a8edb6f`, variant uncommitted on it:

| Strategy | Arm | Wall | UUID |
|---|---|---:|---|
| simple | base | 51.8 s | `8b6a457b` |
| simple | variant | 51.6 s | `d0224a32` |
| simple | variant | 51.8 s | `4b50e8af` |
| simple | base | 51.2 s | `7fc09b8c` |
| complete | variant | 74.0 s | `94d0e635` |
| complete | base | 71.7 s | `32c6baa5` |
| complete | base | 73.4 s | `bf3c1db5` |
| complete | variant | 71.8 s | `4933de4b` |

Neutral on both: the complete pairs disagree in sign, and the first
pair's 2.3 s gap sits in the header-only schedule scan (4.5 s against
6.2 s), I/O the change does not touch.

### Fused simple kind pass (bygg, 2026-11-25, against `a74c2464`)

`brokkr bench --iterations 1 region-extract --mode multi --strategy simple
--dataset planet` (5 strips), interleaved, baseline `--commit HEAD` at
`a74c2464`, variant uncommitted on top of it (the commit that lands
`extract/multi_fused.rs`):

| cell | wall | nodes | ways | relations |
|--|--|--|--|--|
| baseline `615b969f` | 68 s | | | |
| variant `2bd16e7d` | 51.5 s | | | |
| baseline `90f575e0` | 68 s | 15.48 + 23.02 s | 12.78 + 9.82 s | 0.35 + 0.22 s |
| variant `6f12427a` | 51.6 s | 28.86 s | 15.76 s | 0.39 s |

About -24 %, same sign in both pairs. Unlike single extract (about -9 %),
where the fused selection walk cost most of the removed scanner classify,
the multi selection walk keeps the columnar bbox test of the old node
classify and the way walk keeps its membership lookups; what goes is the
second pread, decompression, validation and block build per blob, and the
write pass's per-element membership lookup. The sidecar's per-phase memory
column reads about 0.90 GB in the fused node phase against 0.79 GB in the
old node write phase; schedule scan unchanged (6.5 s).

### Shared complete-ways and smart pass (bygg, 2026-11-25, against `b20ab376`)

Complete and smart multi-extract on sorted input share one
classify and write pass (`extract/multi_dep.rs`), replacing one
`extract_to` per region; cleaned simple joins simple's single pass.
Planet `planet-20260223-reindexed`, `brokkr bench --iterations 1
region-extract --mode multi`, 5 strips, interleaved. Baseline is
`--commit HEAD` at `b20ab376`; the variant is the uncommitted tree on it.

| Strategy | Arm | Wall | Peak anon | UUID |
|---|---|---:|---:|---|
| simple | base | 67.6 s | | `4b50d25e` |
| simple | variant | 68.2 s | | `d3162e42` |
| simple | variant | 67.8 s | | `f0a0f58c` |
| simple | base | 68.0 s | | `e1c35e96` |
| smart | variant | 86.1 s | 0.81 GB | `7539d5e4` |
| smart | base | 395.2 s | 1.38 GB | `d421b98c` |
| complete | base | 334.3 s | | `3e9fdf31` |
| complete | variant | 72.8 s | 0.80 GB | `5f38873a` |

Simple (uncleaned): neutral, medians 67.8 s against 68.0 s, so link 1's
clean branch and the factored-out helpers cost nothing. Smart -78 %
(4.6x), complete -78 % (4.6x), with lower peak anon. Smart variant
phases (`7539d5e4`): schedule scan 6.3 s, node classify 15.2 s, way
classify 13.0 s, relation classify 0.4 s, parents 0.02 s, extra-way
sweep 9.7 s, write 41.4 s. One pair per dependency strategy: a 4.6x
gap needs no more cells.

### Negative IDs as region members (bygg, 2026-11-18, against `c79e6cfd`)

Membership keys pages by the ID's two's-complement bits, so negatives land
in the ordered overflow map and positive input takes the same path minus a
sign check. Japan, 5 strips, simple, `--iterations 1` interleaved: baseline
1964 ms and 1966 ms against variant 1937, 1930 and 1915 ms, neutral. UUIDs
and the matching smart cell are in
[extract.md](extract.md#negative-ids-selected-exactly-bygg-2026-11-18-against-c79e6cfd).

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

## History

### Multi-extract

#### Partial per-region node-blob passthrough disproved (commit `57b01f9`, 2026-04-20, plantasjen)

A shadow counter classified every node blob by whole-blob containment in
each region's bbox without copying anything raw. Planet 5-region
`extract --config --simple` (bbox -25,34,45,72), UUID `dad573cb`: 0 of
32,835 node blobs would have qualified for partial passthrough; all landed
in the none-contained bucket, and the all-regions-contained fast path also
fired 0 times. Verdict: dropped, shadow reverted. The pin comment in
`crates/lib/src/commands/extract/multi.rs` carries the reasoning.

#### Way-classify scratch reuse (commit `b7cd0e4`, 2026-04-17)

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

