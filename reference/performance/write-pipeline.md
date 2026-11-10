# Write Pipeline

### Encode storage kept across blocks (bygg, 2026-11-01, against `5789830e`)

The change cells ran as uncommitted edits on `5789830e`; the change
landed unaltered as the commit after it.

The change: one string table per builder over a byte arena (no
allocation per string, where the old table made an `Rc<str>` per unique
string and freed them all at every block end); ordered-pool workers of
`cat --clean`, the time-filter snapshot path, repack and degrade keep one
`BlockBuilder` for the run and frame each block from its buffer; framing
hands out one exactly sized frame instead of swapped-out header and body
parts that the writer flattened; `take_owned` leaves a capped reservation
for the next block. Outputs of cat `--clean`, repack (cap 8000 and 3000),
degrade and time-filter on germany are byte-identical to `5789830e`.

Arena cap (MALLOC_ARENA_MAX=2) on germany cat `--clean`: `5789830e` 16.9 s
(`42a26ca1`) against 7.4 s uncapped; the change without its string table
15.2 s (8.49 M futex calls at `5789830e` against 7.07 M); with it 7.4 s,
the uncapped wall. The per-blob vectors were a tenth of the cap's cost;
the string table's per-string allocations were the rest.

Planet, interleaved `--bench 1` pairs, clean `5789830e` cells (stored)
against the uncommitted tree (`--force`, not stored); walls in s, peak anon
in MB:

| command | `5789830e` wall | change wall | `5789830e` peak | change peak |
|---|---:|---:|---:|---:|
| cat `--clean version` | 151.1, 153.3 (`f687bd33`, `870e72f6`) | 154.0, 155.3 | 913, 898 | 838, 824 |
| repack | 172.8, 170.6 (`f078a43a`, `43c3f0fc`) | 170.2, 169.0 | 1,934, 1,896 | 1,790, 1,804 |
| degrade `--strip-locations` | 170.5, 171.9 (`e39f176d`, `32b0d647`) | 170.7, 171.3 | 1,914, 1,920 | 1,797, 1,775 |
| time-filter | 134.3, 133.8 (`35a53690`, `ed16f718`) | 132.3, 131.8 | 766, 778 | 720, 719 |
| getparents | 17.4, 15.6 (`a77c851b`, `4ca09dc3`) | 17.4, 15.7 | 283 | 277, 286 |
| apply-changes (4913, `zstd:1`) | 129.5, 128.0, 126 (`86cd89b7`, `3e2ebea2`, `c6312436`) | 106.8, 106.5, 103.3 | 2,435, 2,304, 2,399 | 2,663, 3,270, 3,187 |
| tags-filter (two-pass) | 76.6, 73.2 (`5513882e`, `c9788558`) | 74.2, 73.1 | 2,548 | 2,553, 2,550 |
| multi-extract `--simple` (5 regions) | 128.0, 127.4 (`5d0090fc`, `58f8c8e1`) | 128.6, 127.1 | 9,989 | 9,914, 9,853 |
| cat `--dedupe` | 217.1, 217.7 (`74c1a641`, `c072eb75`) | 215.6, 216.4 | 1,088, 1,086 | 1,131, 1,121 |
| renumber | 131.6, 129.1 (`e785cfa9`, `6091aff9`) | 123.8, 133.2 | 3,314 | 3,331, 3,316 |

Under MALLOC_ARENA_MAX=2 at planet: cat `--clean` 234.5 s (`f38d9391`)
against 153.0 s, repack 266.9 s (`377506c5`) against 173.2 s - the
change's capped walls equal its uncapped ones.

Walls are flat within noise except apply-changes, 17 % faster: an
apply-changes cell of the change with the old `Rc<str>` table ran 124.6 s
and peaked at 2,508 MB, so the wall comes from the arena table. Peaks fall
5 to 8 % on the four commands whose workers now keep a builder; cat
`--dedupe` is about 3.5 % higher. apply-changes' peak rises 0.3 to 0.9 GB,
in a step at the node-to-way transition (51 to 83 threads in both builds)
that stays resident to the end. The table does not hold it: the step is
allocator free memory left by a transient burst there, and the arena table
changes only how much of the burst the arenas' existing free memory
absorbs. "apply-changes' memory step at the node-to-way transition" below
attributes it.

### Dense-node direct encoding, refused (plantasjen, 2026-11-05, against `9ebcfa97`)

`BlockBuilder` stages each dense packed field in a scratch buffer, the
DenseInfo submessage in `elem_scratch` and the DenseNodes body in
`group_buf`, then copies the body once into the encode buffer. The variant
measured every packed body first (a varint-width pass over each dense
vector, checked sums), reserved the whole block once (string table plus
group) and wrote every field straight into the encode buffer. A unit test
against the staged path and a germany `cat --clean version` byte compare
(62,461 blobs, `cmp` clean) showed identical output.

Planet `cat --clean version`, `--bench 1`, interleaved H V V H, after
`fstrim`. H is stored (`41ca4be3`, `f9b8146d`); V ran `--force`, sidecars
`046eceb9` and `095013e5`.

| Cell | Wall | `CAT_NODES` | `CAT_WAYS` |
|---|---|---|---|
| H `41ca4be3` | 296.4 s | 179.2 s | 107.1 s |
| V `046eceb9` | 311.0 s | 190.4 s | 109.6 s |
| V `095013e5` | 312.2 s | 192.0 s | 109.3 s |
| H `f9b8146d` | 304.4 s | 183.7 s | 110.1 s |

The node phase, the only one the change touches, is 5-6 % slower in both
pairs; the way phase is flat. Node-phase peak RSS fell 25-40 MB, inside the
noise floor. About 180 us per 8000-node block across the workers: computing
a width per value costs more than bulk-copying bytes that are already
encoded. Not adopted; the staging stays. Untried shapes: accumulating packed
lengths as values are added, and reserving prefix space then compacting.

## Pipelined writer delivery window (bygg, 2026-10-31)

The pipelined writer admits each item against a delivery window
(`PIPELINE_DELIVERY_WINDOW`, 512 items, returned at ordered handoff).
Before it, finished items behind a slow head were limited only by the
reorder buffer's failure ceiling. Planet, interleaved `--bench 1`, no
fstrim beforehand. Baseline cells are `8aff105a`; variant cells are the
same commit plus the uncommitted change, run with `--force`, so they have
no UUID.

| Command | Cell | Wall | Window wait | Reorder span / filled |
|---|---|---:|---:|---:|
| apply-changes 4913 zstd:1 | `b9c4df3a` base | 126.6 s | | |
| | variant | 128.9 s | 0.06 s | 112 / 112 |
| | `aeaf92e5` base | 141.3 s | | |
| | variant | 128.5 s | | |
| altw external | `115c9b21` base | 437.7 s | | |
| | variant | 455.8 s | 0.012 s | 204 / 181 |
| | `361e31e7` base | 460.5 s | | |
| | variant | 458.5 s | 0.010 s | |

The deltas change sign between pairs, and the producer waited on the
window for a total of 10 to 60 ms per run, so the window costs nothing
measurable on normal input. Raw-send-only commands (cat --clean,
time-filter, getparents) have a reorder span of 1 and cannot reach it.
It does not shrink these runs' peaks: their pile-ups (a reorder span in
the low hundreds, about 25 MB of 146 KB raw items on apply-changes and
about 270 MB of 1.3 MB frames on altw external) sit below the window. It
replaces the unbounded worst case behind a slow head with backpressure.

### Reorder buffer bytes (bygg, 2026-11-04)

`writer_reorder_bytes_high_water` reports the peak output-buffer capacity
held in the pipelined writer's occupied reorder slots, sampled with the
span and filled peaks (definition in `crates/lib/src/write/metrics.rs`). Planet,
interleaved `--bench 1` after `fstrim`. Baseline cells are `184fe3d6`;
variant cells are that commit plus the uncommitted counter, run with
`--force`, so they have no UUID.

| Command | Cell | Wall | Reorder span / filled | Reorder bytes |
|---|---|---:|---:|---:|
| cat --clean | variant | 144.6 s | 1 / 1 | 2.1 MB |
| | `612ed3eb` base | 145.9 s | | |
| altw external | variant | 327.7 s | 231 / 231 | 466 MB |
| | `467a50a9` base | 343.8 s | | |

The variant is not slower in either pair; the altw delta sits in stage 1's
relation scan, before the writer receives anything, so it is drive and
cache state, not the counter. The byte peak is about 2.0 MB per slot,
above the 1.3 MB mean frame, so the earlier slots-times-mean estimate of
about 270 MB undercounted what altw external holds there. cat --clean and
apply-changes now send pre-framed raw items in order from the producer
(`writer_payload_framed_items` 0), so their reorder buffer holds one item
at a time and the counter reads the largest single item.

## `--direct-io` impact summary

| Workload | Bottleneck | `--direct-io` effect |
|----------|------------|---------------------|
| Merge (NA, 18.8 GB) | I/O (concurrent read+write) | **-20%** (uring+none) |
| Merge (Germany, 4.5 GB) | Mixed | Neutral |
| Cat passthrough (planet) | Sequential I/O | +5% slower |
| ALTW (europe) | Memory latency (mmap faults) | +2% slower |
| `cat --dedupe` self-merge (planet) | CPU (sweep), every frame read twice | +16% slower with a direct pass 2 (see below) |

`cat --dedupe` direct pass 2 (bygg, planet self-merge, 2026-10-30, change
on top of `83c3cce1`). With `--direct-io`, HEAD (pass 2 buffered) cell
`ef8f68ee` 265.6 s against an uncommitted direct-pass-2 cell 306.9 s: the
write loop reads 175.7 GB from the device instead of 86.5 GB, because the
self-merge reads each frame once per argument and a direct read gets no
page-cache reuse; average cores 28.1 vs 27.5. The cost is the requested
transport on a doubled-input benchmark shape. The buffered default path
(one branch per frame read) was checked with interleaved `--bench 1`
cells: HEAD `d83a5389` 304.9 s and `1afa4e81` 297.7 s against uncommitted
261.1 s and 259.4 s, identical device reads (86.6 vs 86.5 GB in the write
loop) and pass 1; the gap sits in the CPU-bound write loop and the later
HEAD direct-io cell `ef8f68ee` ran that same buffered pass 2 in 251.7 s,
so it is drift between cells, not the change, and is not claimed.

`sort --direct-io --io-uring` with direct pass-2 copies (bygg, planet,
sorted input, 2026-11-04, change on top of `bc075247`). The copied-through
frames are now read `O_DIRECT` in aligned chunks into the uring writer's
buffers instead of through a second, buffered descriptor. Three
interleaved `--bench 1` pairs, HEAD stored, change `--force`: HEAD
`b9d61181` 45.0 s then change 53.6 s; HEAD `8ed88f79` 57.2 s then change
58.5 s; change 60.6 s then HEAD `fb083b98` 57.1 s. The first pair is the
first cell after a long rest and does not compare. The other two put the
change 2-6 % behind, a sign held across both orders. Pass 1 matches
(6.86 s each) and both write loops read 87.6 GB from the device, so the gap
is the read pattern of the write loop, not more bytes: synchronous direct
chunks get no kernel readahead. Inside the noise band for a single pair,
and the transport is the one `--direct-io` asks for.

`sort --io-uring` against the buffered default (bygg, planet, sorted
input, `bc075247`, 2026-11-04), interleaved stored `--bench 1` cells:
buffered `d18ccf43` 52.1 s, uring `4b51c917` 47.5 s, uring `ac69de9f`
55.8 s, buffered `15393169` 51.5 s. A tie on the median (51.7 s against
51.8 s), with the uring arm's spread wider. Sorted planet is a pure copy of
about 87 GB, and both write loops read it from the device at about
1.8 GB/s with well under one core busy (0.4 buffered, 0.1 uring), so the
drive bounds both arms and the write backend cannot move the wall.

`sort` across the blob-merge engine move, `dd312160` (old) against
`79d2e923` (new) (bygg, planet sorted reindexed input, kernel 7.2.14,
2026-11-08, both arms `--commit` worktrees, buffered, no fstrim: it was
refused in the session). Sequence after a discarded warm-up cell
`ee055efc` (new, 51.8 s, pass 2 44.9 s): old `4f654ece` 75.3 s, new
`b9f304ca` 88.4 s, new `e51b3f6a` 80.2 s, old `fdc99aeb` 78.8 s. Pass 1
(cold header walk) is 6.06-6.08 s in all four. Pass 2 is all of
`writer_write_ns`: old 69.2 / 72.7 s, new 82.3 / 74.2 s, so new minus old is
+13.1 s and +1.5 s in the two adjacent pairs. Counters match on both arms:
50816 blobs, 0 overlap runs, 0 rewritten, one copy-range item of
92010514684 bytes coalescing 50815 frames, 0 userspace-fallback ranges.
Both engines submit the same bulk copy through the same copy loop; the
timing is inconclusive, because every cell after the warm-up ran 50-80 %
slower than it under untrimmed back-to-back 92 GB writes, and the two pair
deltas differ ninefold. Not re-measure-worthy without a trimmed suite and a
reason in the code to expect a pass-2 difference.

## Writer cluster across six commits (bygg, 2026-11-09, `307982d7` vs `52690f1e`)

`307982d7..52690f1e` is six linear commits: one per-block dispatch
(`write_osm_data` over `BlobMeta`, one `reserve`, one `send_raw`), one
`WritePipeline` spawn, one checked frame header encoder
(`encode_frame_header`), writer counters that mean one thing on every
backend, one ordered-delivery loop (`deliver_in_order`) and the OSC gzip
writer reordering with `ReorderBuffer`. No single command reaches all of
it: repack and cat `--clean` frame on their own threads and send
`write_raw_owned`, so they never call `write_osm_data`. Three commands,
both arms `--commit` worktrees, order A B B A, `--bench 1`, untrimmed:

| Command | `307982d7` | `52690f1e` | `52690f1e` | `307982d7` |
|---|---:|---:|---:|---:|
| europe `repack` (raw send, framing, counters, loop) | 151.9 s (`5f0a1965`) | 141.0 s (`8de80460`) | 140.4 s (`99ce34ff`) | 140.9 s (`05e546f7`) |
| germany `extract --simple -b 5.8,47.2,15.1,55.1` (per-block dispatch) | 14.5 s (`2368455a`) | 13.0 s (`9e94ace5`) | 8.68 s (`ce19a942`) | 8.60 s (`ad0ae918`) |
| planet `diff-snapshots --format osc -j 16` (`ParallelGzipWriter`) | 257.4 s (`d673ef92`) | 256.0 s (`a5dd0067`) | 255.8 s (`2d9b1b50`) | 255.5 s (`d17d94db`) |

The germany extract walls fall as the cache warms; one more warm pair,
B then A, gave 8.70 s (`c8f486f7`) and 8.64 s (`5f34dc1a`), so warm B
is about 0.8 % slower: noise. Repack's first cell is the suite's first
and stands apart from the other three. The planet OSC assembly span
(`DERIVECHANGES_WRITE`, the `ParallelGzipWriter` stage) is 8.50 s at A
and 8.23 s at B (`d17d94db` against `2d9b1b50`). Verdict: no detected net
regression for these workloads on the buffered backend. It is a
regression screen, not a per-commit bound: a cost in one commit could
cancel a gain in another. Uncovered: the `pwrite` pool and io_uring
backends (io_uring cannot run on bygg) and the borrowed `BlobMeta::Scan`
and `Bare` branches. Repack at both arms runs under the CLI's two-arena
malloc cap (removed in `25172fc3`); extract and diff never asked for it.

Whole-Europe extract `--simple` is not a usable cell at either arm: both
panic with `reorder buffer window exceeded: seq 65536 is 65536 ahead of
0 (max 65536)`, and the Germany box on Europe panics the same way at
seq 117597, with blob 0 never delivered. Both arms fail identically, so
it is not the writer cluster. HEAD runs the same extract (`dfe4ec94`);
`56a50e0d`, which put every ordered blob pool on one bounded engine, is
the likely fix but was not bisected. Not to be re-measured without a code reason.

`--direct-io` only helps when page cache is a bottleneck (concurrent I/O on
files exceeding available RAM). Sequential reads and memory-bound workloads
are better served by buffered I/O with kernel readahead.

## History

#### Write throughput (commit `d387301`)

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

