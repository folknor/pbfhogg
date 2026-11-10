# Cat

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

The `--clean` path measured below ran `parallel_classify_phase` once per
kind with framed output streamed via `ReorderBuffer` (commits `6184602` +
`b347c0a`, 2026-04-27). Planet `cat --clean version`: **5m34s wall, 750 MB
peak anon** (UUID `f2315551` at `4fc8e35`, 2026-04-27 overnight; previously
`7c4e03eb` 5m48s/835 MB at `b347c0a` re-bench - 14 s wall, 10 % anon
improvement attributable to single-shot variance). The path has since become
one `parallel_classify_phase_ordered` pass per input (every scheduled blob
decoded once, output in input order), so these figures describe the per-kind
one; the one-pass shape is measured in "cat --clean's peak against the README
row".

Sidecar phases across the two shapes: the per-kind shape's `CAT_NODES_*`,
`CAT_WAYS_*` and `CAT_RELATIONS_*` pairs bracketed execution barriers. The
one-pass shape at `8da26b23` replaced them with a single `CAT_FILTERED_*`
pair; a later change (2026-10-26) brought the per-kind names back inside it,
as write-side kind transitions (`KindPhase` in `src/commands/cat/mod.rs`).
Decode of the next kind overlaps a row by up to the pool's admission window
of tasks, and the wait for the first next-kind result lands in the row
before, with no time bound the code enforces. So a per-kind row compared
against this baseline is approximate, the short relation row least of all;
the wall is the exact comparison, and `CAT_FILTERED_*` the exact ordered-pass
total (it excludes the schedule walk and the final flush). Runs from
`8da26b23` until that change carry no per-kind rows.

### cat --clean's peak against the README row (bygg, 2026-11-03, at `1f93e674`)

Planet `cat --clean version`, stored cell `6f9896db`, `--bench 1`, 30
workers: 146 s, 766 MB peak anon. The peak rises through the run and is set
in the `CAT_RELATIONS` segment, the last (segment peaks 289 MB in the pool's
first 112 ms, then 627, 720 and 766 MB). The README's 750 MB is `f2315551`
(plantasjen, `4fc8e35`, the per-kind phases under the two-arena cap, 25
threads at peak against this cell's 33, so 22 workers against 30); this cell
is 2 % above it with 36 % more workers, inside the noise floor. The
encode-storage cells above (824 and 838 MB) ran before the string table
moved into the encode buffer and the worker decode buffers took the pool
limit; which of those moved it was not measured.

The ordered pool's 64-task window does not hold it. Stall counters:
`ordered_pool_result_recv_wait_ns` 130.4 s (89 % of the wall, the consumer
idle for want of a result), `ordered_pool_window_wait_ns` and
`ordered_pool_result_send_wait_ns` 0. The consumer only writes raw frames,
so results in flight stay near one per worker and the window binds only
behind a slow head. Zero window wait says the admission window did not bind
in this run; consumer receive wait is not worker idle time, so it does not
prove no result ever waited behind a slow head. The peak is the workers' own
working sets: decode, read and encode buffers and frames of the blob in
hand.

### cat --clean across the single ordered pass (bygg, 2026-11-09, `a05b3d25` vs `8da26b23`)

Planet `cat --clean version` on `planet-20260223-reindexed`, the parent
(three per-kind phases, a `ReorderBuffer` each) against the commit that made
it one ordered pass with a private 64-slot credit window. Both arms built as
`--commit` worktrees (brokkr#105 cancels), `--bench 1`, A B B A, untrimmed:

| Cell | Commit | Wall | Peak anon | Avg anon |
|---|---|---|---|---|
| `8b442cca` | `a05b3d25` | 229.3 s | 1,043 MB | 941 MB |
| `5cba4939` | `8da26b23` | 231.4 s | 1,104 MB | 902 MB |
| `91b5ebba` | `8da26b23` | 233.3 s | 1,097 MB | 871 MB |
| `c726040f` | `a05b3d25` | 228.7 s | 1,077 MB | 959 MB |

The one pass is 0.9 % and 2.0 % slower in the two pairs, below the noise
floor: no measurable net cost. Peak anon is 2-6 % higher and average anon
4-9 % lower, the per-kind phases' tails replaced by one steady pool. Phase
split (`8b442cca` vs `5cba4939`): schedule walk 6.2 vs 5.4 s, the work 223.1 s
across the three kind phases vs 226.0 s in `CAT_FILTERED`, both at 26-27
cores. Writer stalls match: `writer_recv` 88.5 % of the wall on both,
`writer_pipeline_send` 12.8 vs 12.3 s. Both arms predate `25172fc3`, so both
ran under the two-arena malloc cap; that is why they sit near 230 s against
the 146 s at `1f93e674` above.

Not answered: whether the 64-slot window stalled workers behind a slow head
at `8da26b23`. Its credit gate parks the dispatcher on a condvar and counts
nothing (`ordered_pool_*_wait_ns` arrive with the engine in `c2409c45`), and
a net-neutral wall does not rule out a stall offset by savings elsewhere.
With no regression to attribute, the historical gate was not instrumented.

## Plain cat across the one frame decoder (bygg, 2026-11-09, `2c12a955` vs `307982d7`)

`307982d7` moved the raw-frame walks from a zeroed `vec!` plus
`read_exact` to a capacity-sized `Vec` filled by a bounded `read_to_end`.
`FileReader` implements only `read`, so std's default `read_buf` still
zeroes every payload byte, and a zeroing reader keeps `read_to_end` at
8 KiB-and-doubling read sizes. Plain `brokkr cat --dataset planet --bench
1`, `planet-20260223-reindexed`, both arms `--commit` worktrees, buffered,
order A B B A, untrimmed:

| Arm | Wall | utime | read syscalls | minflt | peak RSS |
|---|---:|---:|---:|---:|---:|
| `2c12a955` (`e304c6b0`) | 42.7 s | 2.32 s | 101,646 | 435k | 80 MB |
| `307982d7` (`451494a3`) | 50.4 s | 2.95 s | 351,017 | 148k | 43 MB |
| `307982d7` (`a124a95e`) | 78.5 s | 3.17 s | 351,017 | 181k | 52 MB |
| `2c12a955` (`8b78f088`) | 84.6 s | 2.55 s | 101,646 | 583k | 79 MB |

The walls climb with every back-to-back 92 GB write and the pair deltas
change sign: no wall difference is detectable. The run is I/O bound
(0.2-0.4 cores, stime flat at about 15 s); the new walk costs about 4.9
more read syscalls and 12 us more user CPU per blob (0.6 s in all) on a
reader thread that is not saturated. Both arms zero the payload, so the
pair does not measure what removing the zeroing would gain.
`FileReader::read_buf` is not writable on stable Rust, and `resize` plus
`read_exact` keeps the zeroing. The pair covers neither
cat's descriptor admission (`56f8d9a9`, an ancestor of both arms, a
per-run cost) nor the reframe's `MAX_BLOB_DATASIZE` compare (`f9625b94`,
in neither arm, and plain cat on indexed input never reframes). Not to be
re-measured without a change to the walk's read path.

The zero-free append that followed (next section) is such a change and was
measured there.

## History

### Cat passthrough (planet 497s → 86.5s arc)

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

