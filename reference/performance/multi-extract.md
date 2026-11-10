# Multi Extract

## Multi-extract

Single-pass simple strategy on sorted input: read PBF once, classify each
element against N regions, compress and frame each region's blocks in the
pool's tasks, write the frames to N sync-mode PbfWriters
(sync-mode writers over the staged files; publication syncs each output
after the final flush, once `MULTI_REL_WRITE_END` and every other phase
marker has ended, between `PUBLISH_START` and `PUBLISH_END`; the `writer_sync_all_ns` and
`writer_sync_all_calls` counters attribute that cost). The rows below
predate the per-output sync and the 256 KiB per-output buffer. 3-phase
barrier (nodes → ways → relations), with per-region `BlockBuilder`s and,
since the shared region membership below, one membership structure per
kind (the rows in this table predate it and used one `IdSet` per region
and kind). 5 disjoint longitude strips per configured bench.

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

