# Sort

## Sort across the blob-merge engine move on unsorted Denmark (bygg, 2026-11-08)

`brokkr sort --dataset denmark --snapshot KEY --bench 1 --commit REF`,
arms `dd312160` (before the move), `79d2e923` (the move: sort and
`cat --dedupe` on one engine, overlap runs streamed from worker threads)
and HEAD `d044980e`, all three `--commit` worktree builds, kernel 7.2.14,
buffered, no fstrim. Three rounds per input with the arm order rotated
(old, move, HEAD; move, HEAD, old; HEAD, old, move). Walls in round order:

| input | blobs, overlap runs (old / move / HEAD) | `dd312160` | `79d2e923` | HEAD | peak anon old / move / HEAD |
|---|---|---|---|---|---|
| `unsorted` | 7396; 3 runs over 6 blobs | 2.55, 2.08, 2.03 s | 2.54, 2.02, 2.08 s | 2.21, 2.03, 1.98 s | - |
| `twin` | 14792; 3 / 7396 / 7396 | 30.6, 29.6, 29.2 s | 6.03, 6.08, 6.01 s | 4.94, 4.96, 4.95 s | 2354 / 791 / 200 MB |
| `chained` | 12487; 3 / 3 / 3 | 31.0, 30.2, 30.3 s | 37.7, 37.6, 37.5 s | 27.3, 27.8, 27.3 s | 2376 / 124 / 46 MB |

UUIDs, in table order. `unsorted`: `1d43e776` `a54557c7` `37ff193a`,
`d30735cf` `92c587ab` `b64a2d8e`, `84a25a94` `fe4316a5` `6e597f1d`.
`twin`: `9ed3f88d` `4e61665b` `33f33c3e`, `74cc5d63` `3357c694` `a2d87928`,
`97141886` `f15dda22` `637079d4`. `chained`: `1230eb36` `73ac97ab`
`d3b85feb`, `720206af` `e9be4e44` `8db6623d`, `d261fa4a` `6ce0b853`
`5ceb86fb`. Peak anon from `brokkr sidecar --stat anon` on `a2d87928`,
`74cc5d63`, `3357c694` (twin) and `8db6623d`, `720206af`, `e9be4e44`
(chained).

The inputs, all with every blob indexed and no sorted claim:

- `unsorted` is `degrade --unsort` of Denmark: one adjacent element pair
  swapped per kind, so at most three two-blob overlap components on this
  otherwise sorted source. It measures pass 1's decode of every payload
  (an unclaimed header) and passthrough; the overlap work is tens of
  milliseconds. Flat across all three arms; the first round's two cells
  ran before the input was warm.
- `twin` is `pbfhogg cat` of the Denmark indexed file with itself: every
  blob overlaps its twin. The old engine grouped adjacent rewrite blobs of
  one kind into a run, so it swept three kind-wide runs and collected
  their output before writing; the move sweeps one run per component (7396)
  and streams. The table measures that whole change, partition included,
  not scheduler overhead on equal jobs.
- `chained` is `pbfhogg cat` of two grow repacks of Denmark at
  `--elements-per-blob 9001` and `10007` (registered as snapshots
  `grow-9001` and `grow-10007`): with no shared interior blob boundary,
  each kind is one connected component, three runs on every arm. Pass 1
  is equal (`SORT_INDEX_BUILD` about 3.65 s); `SORT_PASS2` is 26.5 s old,
  33.9 s move, 24.1 s HEAD (`8db6623d`, `720206af`, `e9be4e44`). The move
  regressed this shape by about 28 % of pass 2 and HEAD is about 9 %
  faster than the old engine. The cause of the regression and of the
  recovery is not isolated (scheduling, buffering, positional reads and
  write overlap changed together at the move; the borrowed-cursor sweep of
  `13ec0927` is a plausible contributor to the recovery, not an
  attribution), and with HEAD ahead of both arms no bisect is warranted.

Every arm keeps both copies of a duplicated element (HEAD's twin output:
104,979,306 nodes, 13,233,052 ways, 92,206 relations, twice Denmark);
output bytes differ legitimately across the move, which flushes a block
at each component boundary. Element contents were not compared across
arms here; `brokkr verify sort` is the correctness check.

Pass 2 runs one sweep per component, so parallelism on unsorted input is
bounded by the component count: on `chained` three sweeps cover the whole
file and the node component dominates.

## sort --force on unindexed Europe (bygg, 2026-11-08)

`060c2f74` made sort's pass 1 scan each unindexed blob's tag keys along
with its IDs and hold the serialized key set to pass 2, so a reframed
passthrough carries tagdata as plain `cat` writes it. Measured
`060c2f74^` (`9744a9fc`) against `060c2f74` with the `sort-cold` script
bench (`benches/sort/sort-cold.lua`, `--force yes`), both arms `--commit`
worktrees, alternated A B B A, one iteration per invocation, input
evicted and checked 0 B resident by fincore. Input: europe `pbf.raw`
(`europe-20260301-seq4714.osm.pbf`, 31.6 GB, 522,168 blobs, header
claims `Sort.Type_then_ID`, no indexdata). Every arm: all 522,168 blobs
passthrough, none overlapping, rewritten or restructured.

| Arm | Run | SORT_INDEX_BUILD | Pass 2 | Child wall | Peak anon | Output bytes |
|---|---|---|---|---|---|---|
| `9744a9fc` | `7eeed226` | 164.66 s | 14.97 s | 179.6 s | 59.7 MB | 33,981,195,365 |
| `060c2f74` | `35892bd8` | 167.63 s | 19.24 s | 186.9 s | 954.6 MB | 34,858,236,257 |
| `060c2f74` | `224bd93a` | 165.12 s | 15.27 s | 180.5 s | 956.1 MB | 34,858,236,257 |
| `9744a9fc` | `ec335d61` | 161.57 s | 15.03 s | 176.6 s | 58.8 MB | 33,981,195,365 |
| `fc076a09` (HEAD) | `13828c41` | 170.64 s | 15.30 s | 186.0 s | 954.6 MB | - |

- Index build: median 163.1 s against 166.4 s, +2.0 %, the noise floor.
  Pass 1 runs at 0.8 cores in both arms: the payload scan was already
  serial before `060c2f74`, which added the tag-key marking and
  serialization to it, not the serial read. The HEAD cell is one sample
  and sits inside the band.
- Peak anon: +~895 MB, matching the 877 MB of tagdata the output gained
  (about 1.7 KB per blob). The key sets live from pass 1 until the plan is
  dropped after pass 2; `plan`'s four per-frame flag vectors are planning
  scratch (about 2 MB here) and are gone before pass 2.
- Verdict: no wall cost; a memory cost equal to the output's tagdata,
  with no fixed bound (it scales with reframed passthrough content, so the
  Europe ratio of 2.6 % of input is an observation, not an envelope).
  Accepted on this path; the rationale and the spill remedy are at
  `Reindex` in `crates/lib/src/commands/blob_merge.rs`. Planet `pbf.raw` (50,816
  blobs) was not measured.

### Pass 1 scans on a worker pool (bygg, 2026-11-09, against `252ad27a`)

Pass 1's payload scans (pread, decompress, ID and tag-key scan, the
reframing decision) moved to an ordered worker pool; the header walk and
the in-order fold stay on the calling thread, and believed blobs travel
in batches with no payload read. Same `sort-cold` bench and input as
above; baseline `--commit 252ad27a` worktree, variant the uncommitted
working tree (`--force` runs, numbers from the run output and sidecar).

| Arm | Run | SORT_INDEX_BUILD | Pass 2 | Child wall | Peak anon | Output bytes |
|---|---|---|---|---|---|---|
| variant | `663943f9` | 14.62 s | 21.68 s | 36.3 s | - | 34,858,236,257 |
| `252ad27a` | `19a8bdfa` | 164.75 s | 15.10 s | 179.9 s | 949 MB | - |
| variant | `e3e28886` | 13.56 s | 11.93 s | 25.5 s | 1,073 MB | 34,858,236,257 |

- Index build: 164.7 s to about 14 s (11x); the sort's wall 180 s to
  26-36 s. Pass 2 is untouched; its spread across the variant cells is
  page-cache carry-over from pass 1, not code.
- Peak anon: about +125 MB, the workers' read and decompression scratch.
- The believed path (indexed europe, `brokkr sort --bench 1`, header-only
  walk), alternated variant / base / variant / base: SORT_INDEX_BUILD
  14.0 / 7.0 / 8.6 / 10.3 s (`cc4fd3d7`, `a5dabcf8`, `8d2bb173`,
  `c5fd3184`). The signs disagree across the pairs, so no cost is shown;
  that walk is I/O bound and swings with cache state in both arms.

## History

### Sort

`pbfhogg sort` repairs unsorted PBFs into `Sort.Type_then_ID` order via a
two-pass blob-level permutation sort: pass 1 walks blob headers and builds
an index of `(element_type, min_id, max_id)`; pass 2 raw-passthroughs
non-overlapping blobs and decode-merges overlapping ones through a binary
heap. **Production reality: Geofabrik / planet input already ships
sorted**, so overlap count is ~zero and pass 2 is pure blob-level
`copy_file_range` passthrough - the command runs as a verify-and-reframe
step, not a sort. The genuinely-unsorted path (osmosis output, hand-edited
fixtures) is the only scenario exercising the decode-merge path. Its
benchmark inputs are Denmark snapshots (`unsorted`, `twin`, `chained`);
reference/performance/sort.md, "Sort across the blob-merge engine move on
unsorted Denmark", has what each one exercises.

Optimization arc drafted 2026-04-23 in a sort plan note (retired 2026-07-13;
this is its durable record). All measurements plantasjen, NVMe->NVMe on one
ext4 partition (`/dev/nvme1n1p1`; the `target=hdd` label in `brokkr env` is
the cargo `target/` dir, not the sort scratch path).

#### Landed opportunities

- **#1 `copy_file_range` coalescing (commit `244c6ec`).** Transplanted the
  apply-changes drain coalescer: track an in-flight `(start, end)` range,
  extend on contiguous-in-input blobs, flush as one `write_raw_copy` on
  break. On already-sorted input the whole file collapses to a single CFR
  call. Europe (`740ed14f`): producer syscalls 522,168 -> 1,
  `sort_copy_range_coalesced` 522,167, `writer_pipeline_send_wait_ns` 35 s
  -> 2.65 us. **Wall did not move** (53.0 -> 56.3 s, single-sample) because
  the writer was already drain-limited by ext4 in-kernel CFR bandwidth -
  the coalescer shifts time from `SORT_WRITE_LOOP` to `SORT_FLUSH` without
  shortening either thread's real work. Useful for any future change that
  unpins the writer.

- **#4 `HeaderWalker` pass 1 (commit `1f97fae`).** Migrated pass 1 from
  `FileReader` (BufReader + `fadvise(SEQUENTIAL)`) to `HeaderWalker` (pread
  + `fadvise(RANDOM)`). Splits hard on blob density:

  | scale | pass-1 disk read | pass-1 wall | total wall | verdict |
  |---|---|---|---|---|
  | europe (522k blobs) | ~34 GB -> 2.86 GB (-91%) | 16-18 s -> 27.4 s (+49%) | 56.3 -> 68.0 s (+21%) | regression |
  | planet (50k blobs) | 32 GB -> 674 MB (-98%) | 18.94 -> 6.73 s (-65%) | 135.1 -> 123.3 s (-9%) | win |

  Europe regresses because 522k serial QD=1 preads (~50 us NVMe
  random-read latency each) lose to BufReader's overlapped readahead;
  planet wins because at 50k blobs the walk is cheap AND the pass-2
  page-cache-thrashing shutdown cost (`SORT_PASS2_END`: 32,788 majflt /
  2.56 s, process pages evicted by the write loop) vanishes. **Production =
  planet, so the walker is a net win in the scenario that matters.** The
  cleanup-vanish is probabilistic (cache-state dependent): later planet
  runs on `68e1ba0` saw `SORT_PASS2_END` return at ~2 s / ~31k majflt.

- **#3 parallel overlap-rewrite in pass 2 (2026-04-24).** Overlap runs
  (kind-bounded, self-contained) parallelise cleanly via
  `overlap_runs.par_iter()` producing `Vec<OwnedBlock>` buffered before the
  serial write loop; zero-cost on already-sorted input
  (`overlap_runs.is_empty()` short-circuits). Predicted 1.5-3x on overlap
  work, unverified at the time (no unsorted input was configured then) and
  never measured against the serial sweep before it. Memory: buffered overlap
  output is uncapped; pathological unsorted input could approach input size
  (fix if it bites: bounded rayon pipeline + reorder buffer).
  **Superseded 2026-10-25** (measured afterwards, 2026-11-08, see
  reference/performance/sort.md "Sort across the blob-merge engine move on
  unsorted Denmark"): sort and `cat --dedupe` became one
  engine (`crates/lib/src/commands/blob_merge.rs`), whose overlap runs are swept on
  dedicated worker threads and stream their blocks to the write loop in run
  order (a bounded channel per run, an admission window of two runs per
  worker), so buffered overlap output is bounded and the write loop overlaps
  the sweeps. The `SORT_OVERLAP_PARALLEL_START`/`_END` markers went with
  the collect-then-write phase; `SORT_WRITE_LOOP` now spans the sweeps.
  A run is now one overlap component; it was a maximal stretch of adjacent
  rewrite blobs of one kind, so input whose every blob overlaps a
  neighbour (a file merged with itself) had at most one run per kind, so
  the old `par_iter` ran at most three jobs, each swept serially.
  Workers read the pass-2 source by position instead of opening the input
  once per run. On already-sorted input (no runs) nothing starts and the
  pass is the coalesced copy as before. Later (2026-10-26, unmeasured) the
  runs moved onto the ordered pool engine's streamed entry point
  (`ordered_pool::run_ordered_streams`): the write loop issues runs itself,
  so the dispatcher thread, the admission gate and the order channel went,
  with the same window and per-run depth, and pass 2 now records stall
  counters (see "Worker-pool unification" below).

#### Do-not-reattempt

- **Streaming `fadvise(DONTNEED)` on a BufReader walk ("M3").** Tried
  2026-04-24 on top of `1f97fae`: europe 53.8 s (-21% vs walker, cleanup
  majflt 35k -> 0) but planet **136.3 s (+11% vs walker)** because it reads
  the whole file and gives up the walker's pass-1 IO reduction (20 s
  BufReader vs 6.7 s walker on planet). Zero-sum across europe+planet;
  walker wins the tiebreaker on the production scale.
- **Parallel-writer CFR chunking.** Parked, <=1.18x (above).
- **Pipelined -> sequential decode conversion.** Off the table per the
  project-wide anti-conversion rule
  (`reference/pipelined-reader-paths.md`); sort uses direct pread per blob,
  correct for the two-pass shape.

The walk term behind the europe pass-1 regression, why batched exact
probes cannot flatten it, and the cold read-ahead the walker now does on
files of small blobs are in `reference/blob-density.md` "The walk term";
sort pass 1 on europe has not been re-measured since.

#### Correctness: intra-blob disorder (fixed 2026-07-11)

Blob-level permutation sort assumed every blob is internally sorted and
nothing checked it: a file whose blobs are internally unsorted but whose
blob `(min_id, max_id)` ranges do not overlap passed through byte-identical
and stamped `Sort.Type_then_ID` - silent corruption. Fixed by
`scan_block_ids_checked` (tracks intra-blob monotonicity during the min/max
scan), keyed on the header's `Sort.Type_then_ID` claim rather than
indexdata presence; any out-of-order blob is folded into pass 2's decode +
re-encode. Full ruling in CORRECTNESS.md; tests in `crates/cli/tests/cli_sort.rs`.

---

#### Planet writer-side floor (storage-stack bound, not software)

At planet the producer does one syscall and the writer does all the wall.
Buffered: ~116 s of in-kernel ext4 CFR at ~800 MB/s (92 GB / 114.9 s).
`--io-uring` (`7f6288c0`): ~111 s of ReadFixed+WriteFixed at ~827 MB/s,
total 118.1 s (**-4%**) - but reintroduces the `SORT_PASS2_END` page-cache
thrash (25,478 majflt / 1.65 s) that buffered post-walker eliminated,
because uring reads payload bytes through the page cache while ext4
in-kernel CFR bypasses it. Buffered stays the default; `--io-uring` only
when 4% beats cache cleanliness.

Both paths are single-thread-bound. An ext4 CFR concurrency probe
(`probe_ext4_cfr.py`, deleted after use) measured 1 vs 2 concurrent
`copy_file_range` on the same partition: 792 -> 931 MB/s aggregate
(466 MB/s per thread under contention) = **1.18x**, ~21 s theoretical
planet savings. The probe was optimistic (different inodes = less
contention than same-file-different-offset), so real parallel-writer
chunking scales <=1.18x. **#2 parallel-writer chunking parked**: days of
`parallel_writer.rs` restructuring for <18% wall. The ~800 MB/s ceiling is
an ext4-CFR characteristic, not pbfhogg code.

Planet hotpath (`e42b0c8c`) + alloc (2026-04-27 at `4fc8e35`, UUIDs
`d64932d2` / `26fb329e`): 115.4 s wall, **94% in the pool worker's
`copy_range_to_fd`** (unannotated; the pool's copy loop, since folded
into `write::copy_range::copy_range`) inside `writer.flush()`; main thread
Blocked 20% avg CPU waiting on the writer drain. No allocation pressure
(459 MB exclusive, all `blob_wire::parse`; net diff 78.6 MB). Nothing to
chase in software on this storage stack.

