# `sort` - optimization plan

Target: `pbfhogg sort` - repairs unsorted PBFs into `Sort.Type_then_ID`
order. Two-pass blob-level permutation sort: pass 1 scans all blobs
and builds an index of `(element_type, min_id, max_id)`; pass 2
raw-passthroughs non-overlapping blobs and decode-merges overlapping
ones through a binary heap.

Drafted 2026-04-23 from a fresh read of
[`src/commands/sort/`](../src/commands/sort/) against the modern
pipeline primitives documented in
[`reference/pipeline.md`](../reference/pipeline.md) and
[`reference/pipelined-reader-paths.md`](../reference/pipelined-reader-paths.md).

## Current state (2026-04-24)

Host `plantasjen`, I/O paths both on `/dev/nvme1n1p1` (ext4) - the
`target=hdd` drive label in `brokkr env` is misleading for these
benches, see "NVMe target correction" below.

| scale  | mode    | uuid       | commit    | wall    | notes |
|--------|---------|------------|-----------|---------|-------|
| europe | bench   | `043cf4b6` | `b891514` | 53.0 s  | baseline, pre-coalesce |
| europe | alloc   | `99c58e53` | `b891514` | 54.7 s  | alloc-instrumented |
| europe | hotpath | `fd2ef4e7` | `b891514` | 64.7 s  | hotpath-instrumented |
| europe | bench   | `740ed14f` | `244c6ec` | 56.3 s  | post-coalesce, `--bench 1` |
| europe | bench   | `25d71ce7` | `1f97fae` | 68.0 s  | post-walker, europe-scale regression (+21 %) |
| planet | bench   | `1aef9d9c` | `244c6ec` | 135.1 s | pre-walker, planet baseline |
| planet | bench   | `bb392a17` | `1f97fae` | **123.3 s** | **post-walker, -9 % vs pre-walker** |
| planet | bench   | `7f6288c0` | `1f97fae` | **118.1 s** | post-walker `--io-uring`, -4 % vs buffered |
| planet | hotpath | `e42b0c8c` | `1f97fae` | 119.3 s | post-walker hotpath, main thread blocked 80 % on writer |

Europe shows a +21 % wall regression on `1f97fae`; planet shows a
-9 % wall *improvement* on the same commit. The walker is
net-positive at production scale. Denmark (indexed, sorted) 366 ms,
Japan (indexed, sorted) 1.33 s,
[`reference/performance.md:774`](../reference/performance.md#L774).

### Baseline anatomy (`043cf4b6`, pre-coalesce)

Phase split:

- `SORT_INDEX_BUILD` 16.39 s (30.9 %)
- `SORT_OVERLAP_DETECT` 30 ms
- `SORT_WRITE_LOOP` 35.01 s (66.1 %)
- `SORT_FLUSH` 905 ms

Counters confirm the already-sorted path: `sort_blobs_passthrough =
522168`, `sort_blobs_overlap = 0`, `sort_blobs_rewritten = 0`, 35.26
GB in = 35.26 GB out. The writer issues 522 168 single-blob
`copy_file_range` calls (`writer_payload_copy_range_items`), one per
blob. **`writer_pipeline_send_wait_ns = 34.97 s` ≈ `SORT_WRITE_LOOP`**:
every `tx.send` on the bounded pipeline channel blocked - the writer
thread was saturated, the producer was not. `SORT_WRITE_LOOP` logs
**519 673 voluntary context switches** on the main thread, one per
blocked send.

Hotpath confirms the shape: `write_passthrough_blob` is 64.4 % of
wall (522 168 calls, avg 79.7 µs / p50 13.6 µs / p95 316.7 µs),
`build_blob_index` 29.6 %, `blob_wire::parse` 0.41 %. Alloc profile:
`blob_wire::parse` owns 834.9 MB across 522 171 calls (~1.6 KB each,
short-lived, net diff 78.6 MB); `write_passthrough_blob` is zero-byte
exclusive. No allocator pressure worth chasing.

**The production scenario is already-sorted input.** Geofabrik and
planet PBFs ship in `Sort.Type_then_ID`; every pbfhogg pipeline step
preserves that order (per
[`reference/pipeline.md:231`](../reference/pipeline.md#L231)). On
already-sorted input pass 1's index detects zero overlapping blob
pairs, pass 2 is a pure blob-level raw passthrough, and the command
serves as a verify-and-reframe step rather than a real sort.

The genuinely-unsorted case (osmosis output, custom exporters,
hand-edited fixtures) is the only scenario that exercises the
decode-merge path. That case has no current benchmark and is low
priority.

### Post-coalesce anatomy (`740ed14f`, commit `244c6ec`)

After landing the coalescer (opportunity #1, below):

- `SORT_INDEX_BUILD` 18.42 s
- `SORT_WRITE_LOOP` **1 ms**
- `SORT_FLUSH` **37.74 s**
- `sort_copy_range_calls = 1`, `sort_copy_range_coalesced = 522 167`
- `writer_payload_copy_range_items = 1`
- `writer_pipeline_send_wait_ns = 2 650 ns` (from 34.97 s)
- `writer_write_ns = 36.41 s` (from 34.72 s)

Producer-side accounting that improved:

- syscalls: 522 168 → 1
- `SORT_WRITE_LOOP` main-thread vol_cs: 519 673 → 0 (loop is 1 ms)
- `SORT_PASS2_END` majflt: 2 927 → 0 (baseline saw faults after the
  35 GB write loop thrashed the page cache and evicted process
  pages; with one giant CFR the cache pattern shifts and that
  shutdown cost vanishes)
- `writer_pipeline_send_wait_ns`: 35.0 s → 2.65 µs

What didn't move: **wall time** (53.0 s → 56.3 s, single-sample,
inside run-to-run noise but directionally flat). The bottleneck was
already the writer thread - `pipeline_send_wait = 34.97 s` over a
35.00 s WRITE_LOOP proves the channel was full continuously, so
collapsing the producer's 522 k sends into one shifts time from
`SORT_WRITE_LOOP` to `SORT_FLUSH` without making either thread's real
work shorter. The writer's throughput ceiling here is in-kernel ext4
`copy_file_range` on NVMe at ~600-800 MB/s - not syscall overhead,
not HDD bandwidth (see "NVMe target correction" below).

Peak RSS also moved +100 MB (805 → 911 MB pass 1; 817 → 913 MB pass
2) with no obvious code reason. Most likely allocator watermark under
a different request pattern - worth watching on planet, not worth
chasing on europe.

### Post-walker anatomy (`25d71ce7`, commit `1f97fae`) - europe regression

After migrating pass 1 to `HeaderWalker` (opportunity #4, below):

- `SORT_INDEX_BUILD` **27.41 s** (from 18.42 s, **+49 %**)
- `SORT_FLUSH` 38.38 s (unchanged from 37.74 s)
- Total wall **68.0 s** (from 56.3 s, **+21 %**)

IO profile shifted as intended:

- pass 1 disk read: **2.86 GB** (from 28.19 GB / 34.10 GB on the two
  earlier runs - a 10-12x reduction, exactly what `fadvise(RANDOM)` +
  pread-only headers should produce)
- pass 1 vol_cs: 365 653 (from 103 530 / 123 170 - ~3x more because
  each pread blocks at queue depth 1 where BufReader's readahead
  batches amortise sync waits across 256 KB)

The migration eliminated page-cache pollution (35 GB of payload
never touched) at the cost of wall: 522 168 serial 4 KB preads at
~50 µs each of NVMe random-read latency = ~26 s of unhidden sync
waits, vs BufReader + `fadvise(SEQUENTIAL)`'s ~16 s of overlapped
readahead for the same walk.

Pass 2 did **not** get faster from the freed page cache. Pass 2
uses in-kernel `copy_file_range` on ext4, which bypasses the user
page cache for the copy itself, and even if it did reuse cached
pages the 30 GB RAM host has long since evicted pass 1's readahead
by the time pass 2 reaches them. So the IO-reduction win is pure
cost on this europe benchmark: more wall, same pass-2 time.

### Planet anatomy (`1aef9d9c` → `bb392a17`) - walker wins at scale

Same A/B on the planet dataset (92 GB input):

| phase                    | pre-walker `244c6ec` | post-walker `1f97fae` | delta |
|--------------------------|----------------------|-----------------------|-------|
| `SORT_INDEX_BUILD`       | 18.94 s              | **6.73 s**            | **-65 %** |
| pass 1 disk read         | 31.94 GB             | **674 MB**            | **-98 %** |
| pass 1 vol_cs            | 87 191               | 90 990                | flat |
| `SORT_FLUSH`             | 113.60 s             | 116.43 s              | +2.5 % (noise) |
| `SORT_PASS2_END` cleanup | 2.56 s, 32 788 majflt | **absent**            | **vanished** |
| total wall               | 135.1 s              | **123.3 s**           | **-9 %** |

Three things moved cleanly in the right direction:

- **Pass 1 IO:** 32 GB → 674 MB. Bigger ratio than europe (-98 % vs
  -91 %) because planet has a higher blob-count-to-bytes ratio and
  the walker reads only headers.
- **Pass 1 wall:** 18.94 s → 6.73 s, *down*, not up. The europe
  regression model (QD=1 pread latency losing to BufReader readahead)
  predicted roughly +40 s on planet extrapolating europe's per-blob
  cost linearly. That didn't happen. Two factors plausibly combine:
  (i) page cache for the planet header regions was warm from
  indexdata generation earlier in the session - 674 MB of physical
  disk read for ~1.3 M blobs × 4 KB pages = ~17 % of blob-header
  pages actually faulted, the rest hit cache at ~µs latency;
  (ii) SORT_INDEX_BUILD is walltime, not CPU - pre-walker's 18.94 s
  already included plenty of BufReader-imposed stall, so the absolute
  headroom for pread to lose was smaller than europe suggested.
- **`SORT_PASS2_END` cleanup vanished:** the 32 788 majflt and 2.56 s
  of shutdown cost in the pre-walker run - process pages evicted by
  the pass-2 write loop's page-cache fill - disappeared entirely in
  post-walker. Exactly what notes predicted for europe but didn't
  see there; on planet at 3x the byte volume the effect shows up
  plainly.

Pass 2 (`SORT_FLUSH`) is flat at ~115 s - bandwidth-limited by
in-kernel ext4 `copy_file_range` on NVMe (see "NVMe target
correction" below), insensitive to what pass 1 did with the page
cache.

Caveat on warm-cache influence: the 674 MB figure is consistent with
~17 % of header pages faulting, so cache state accounts for part of
the pass-1 win. A cold-cache planet run would land pass 1 closer to
25-35 s (scaling europe's per-blob miss cost), so the *warm-cache*
post-walker is ~20 s faster than a cold-cache one would be. Even at
the pessimistic end (pass 1 = 35 s), total planet wall would be
~152 s vs 135 s pre-walker - a ~13 % regression, still smaller than
europe's +21 % and likely small enough to accept as the price of the
IO reduction. A `drop_caches` planet bench on `1f97fae` would close
this out; parked as future work, not blocking.

### `SORT_PASS2_END` majflt variability on planet

The "cleanup phase vanishes" bullet above came from `bb392a17`
(walker baseline), which showed `SORT_PASS2_END` as 0 ms / 0 majflt.
Later planet runs on `68e1ba0` (post-opp-#3) show the cleanup phase
back at **~2 s and ~31 000 majflt** - effectively the pre-walker
shutdown cost returning.

This is **not** opp #3's doing: planet has zero overlap runs, the
rayon par_iter short-circuits (`sort_overlap_runs=0`), and the
main pass-2 loop is structurally identical to the pre-opp-#3
commit. All counters (`sort_copy_range_calls=1`,
`writer_payload_copy_range_items=1`, `writer_flush_ns≈115 s`,
`writer_write_ns≈114 s`) match bb392a17 within run-to-run noise.
The `SORT_PASS2_END` region runs entirely after `writer.flush()`
has already completed and fsynced - it's process teardown, not
sort work.

Most likely it's cache-state timing: bb392a17's "no cleanup
cost" was a favourable cache window where process code pages
happened to survive pass-2's 92 GB of buffered-write pressure.
`68e1ba0` runs hit the unlucky case where pass-2 evicted some
of the pbfhogg code region and exit-time closers page them
back in. The walker commit's claim that HeaderWalker's
bounded cache footprint eliminates shutdown thrashing isn't
reliably true at planet scale - it's a probabilistic win that
depends on what else is in the page cache at end-of-pass-2.

Action: none right now. Watch the counter on future planet
runs; if it stays in the ~30 k range, consider amending the
opp #4 takeaway (walker as "net win") to reflect the
probabilistic cache-hygiene property. If a future change
drives it back to zero reliably, that's the genuine
improvement.

### NVMe target correction (2026-04-24)

Earlier drafts of this note described pass 2 as "HDD-EXDEV
bandwidth-limited" - based on `brokkr env` reporting
`target=hdd`. That was wrong. The actual bench paths are:

- input: `data/planet-...osm.pbf` → `/dev/nvme1n1p1`
- output: `data/bench-tmp/bench-sort-output.osm.pbf` → `/dev/nvme1n1p1`

Same NVMe ext4 partition. `copy_file_range` stays in-kernel; EXDEV
fallback (`copy_range_fallback_pwrite`, 256 KB pread+write) never
triggers. The `target=hdd` drive label in `brokkr env` corresponds
to the `target/` directory (cargo build artifacts on HDD) and is
not the scratch path the bench uses. The scratch drive label
(`scratch=nvme`) is the one that matters for sort I/O.

Writer counters on `bb392a17` confirm: `writer_payload_copy_range_items=1`
accepting 92 GB, `writer_write_ns=114.94 s`, all channel-wait
counters near-zero. 92 GB / 114.9 s ≈ **800 MB/s in-kernel ext4 CFR**
- that's the bandwidth the writer actually saw. No EXDEV, no HDD.

Implications for opportunity #2 ("Writer-side throughput"):
- The NVMe→NVMe comparison listed there is *this* bench. No need
  to re-bench on different storage.
- The 800 MB/s ceiling is an ext4 CFR characteristic, not pbfhogg
  code. ext4 CFR does in-kernel copy (no reflink support). A
  reflink-capable fs (btrfs, xfs with `reflink=1`) would collapse
  the same op to O(1) metadata and pass 2 would drop to <1 s -
  but that's an fs migration, not a code change.
- Parallel writer chunking (round-robin the coalesced CFR across N
  workers) still has theoretical upside: one giant CFR pins one
  worker thread; chunking lets 4 workers issue CFRs concurrently,
  potentially approaching the NVMe device's raw bandwidth (several
  GB/s) rather than the single-thread ext4 CFR ceiling. Size
  against an actual measurement before committing days.

### io_uring anatomy (`7f6288c0`, commit `1f97fae`)

Planet post-walker with `--io-uring`:

| phase             | buffered `bb392a17` | io_uring `7f6288c0` | delta |
|-------------------|---------------------|---------------------|-------|
| `SORT_INDEX_BUILD`| 6.73 s              | 5.05 s              | -1.7 s |
| `SORT_FLUSH`      | 116.43 s            | **111.23 s**        | -5.2 s |
| `SORT_PASS2_END`  | absent              | 1.65 s, 25 478 majflt | **+1.65 s, thrashing returns** |
| total wall        | 123.3 s             | **118.1 s**         | **-4 %** |
| peak threads in FLUSH | 2               | 67                  | uring SQPOLL / kernel threads |

Writer counters on `7f6288c0`:
- `writer_uring_submit_calls` = 350 993
- `writer_uring_submit_and_wait_calls` = 14 009
- `writer_uring_submit_ns` = 406 ms
- `writer_uring_cq_wait_ns` = 1.09 s
- `writer_write_ns` = 111.2 s (≈ `flush_ns`)
- `writer_payload_copy_range_items` = 1 (accepts op) but
  `handle_copy_range_uring` internally chunks to 256 KB
  ReadFixed+WriteFixed pairs - no `copy_file_range` syscall issued
- `SORT_FLUSH` disk read = 87 GB (confirms the uring path reads
  input bytes through the device, unlike ext4 in-kernel CFR which
  bypasses the read_bytes counter)

So io_uring saturates the NVMe differently: ~92 GB / 111.2 s ≈
**827 MB/s** aggregate, marginally above the buffered path's
~800 MB/s. Both paths are single-thread-bottlenecked (one pool
worker holds the sole `OutputChunk::CopyRange`), io_uring just
gets slightly better per-thread throughput via registered buffers
and sq/cq overlap.

Trade-off: io_uring reintroduces the `SORT_PASS2_END` page-cache
thrashing cost (25 478 majflt, 1.65 s) that the post-walker
buffered path eliminated - because uring reads planet payload
bytes through the page cache (87 GB Disk Read in SORT_FLUSH),
whereas ext4 in-kernel CFR bypasses the user page cache entirely.
Net win is 4 %, but the "cleaner cache behaviour" property of
post-walker buffered is partly given back.

### Hotpath anatomy (`e42b0c8c`, commit `1f97fae`)

Planet post-walker with `--hotpath`. (At the time of this run the
2x-input memory preflight in brokkr would have rejected planet,
so the bench was originally captured under `--no-mem-check`; the
preflight was later removed entirely. Sort itself is O(num_blobs)
memory regardless, so the run never needed the override.)

Wall 119.3 s. Top annotated frames:

| frame                          | calls | total    | % wall |
|--------------------------------|-------|----------|--------|
| `pbfhogg::main`                | 1     | 119.24 s | 100 %  |
| `sort::sort`                   | 1     | 119.24 s | 100 %  |
| `sort::build_blob_index`       | 1     | 4.31 s   | 3.6 %  |
| `read::blob_wire::parse`       | 50 819| 111 ms   | 0.09 % |
| `write::framing::frame_blob_into` | 1 | 71 µs    | ~0 %   |

The remaining ~96 % (≈115 s) is outside any `#[hotpath::measure]`
annotation - specifically, inside the pool worker's
`copy_range_to_fd` in `parallel_writer.rs:364`, which isn't
annotated. `writer.flush()` itself isn't annotated either.

Thread view: main thread status **Blocked 20 % avg CPU** (peak
64 % during pass 1), the rest waiting on the writer channel
drain. This confirms what the metrics already showed
(`writer_flush_ns = 116.4 s`, main thread spending nearly all its
time in `writer.flush()`). Hotpath adds no new information here -
the hot frame is in the worker thread and isn't decorated.

If we want hotpath-level granularity inside the writer, candidates
to annotate:
- `copy_range_to_fd` (`parallel_writer.rs:364`)
- `copy_range_fallback_pwrite` (only active on EXDEV - not this
  bench)
- `handle_copy_range_uring` (`uring_writer.rs:416`)
- `PbfWriter::flush` (`writer.rs`)

Not worth adding for opp #2 sizing - the wall-clock counters
already localise the 115 s to a known code region. Annotate only
if we need per-chunk timing distribution inside the copy loop.

### ext4 CFR concurrency probe (2026-04-24)

Ran a minimal probe before committing to opp #2's parallel-writer
chunking: `os.copy_file_range` (same syscall as
`parallel_writer::copy_range_to_fd`) of the planet PBF (86 GB) to
one output vs two concurrent outputs on the same NVMe ext4
partition. Probe script was `probe_ext4_cfr.py` at repo root,
deleted after use.

| configuration       | wall     | per-thread | aggregate |
|---------------------|----------|------------|-----------|
| 1 thread            | 110.75 s | 792 MB/s   | 792 MB/s  |
| 2 threads concurrent | 188.49 s | 466 MB/s   | **931 MB/s** |

**Aggregate speedup: 1.18x.** The 792 MB/s figure matches sort's
measured in-kernel CFR throughput (800 MB/s on `bb392a17`) to
within noise - so the sort writer is already hitting ext4's
single-thread CFR ceiling, and concurrent CFR from two threads
only adds 18 % aggregate. Per-thread throughput drops from
792 to 466 MB/s under contention, showing the second thread is
contending for *something* below the syscall layer (ext4 journal,
block layer queue, NVMe controller - this probe can't localise
further, and it doesn't need to).

The probe is **optimistic** for opp #2: it copies to two
different inodes, whereas parallel-writer chunking would target
the same output file at different offsets - same inode, single
extent allocator, likely more contention. So real-world opp #2
scaling would be ≤1.18x.

Conclusion: opp #2 parks. ~21 s theoretical wall savings on
planet (116 s pass 2 → ~95 s) is not worth days of code that
restructures `parallel_writer.rs` dispatch. Bigger wins live
elsewhere - see opp #6.

### Takeaway

- **Opportunity #1** (coalescer) is mechanically correct and landed:
  producer is now O(runs) syscalls instead of O(blobs), and the
  accounting (vol_cs, majflt at shutdown, send-wait) reflects that
  cleanly. Wall didn't move on europe because the writer was already
  drain-limited (`pipeline_send_wait = 34.97 s` over a 35.00 s
  WRITE_LOOP); planet confirms the same shape (`SORT_FLUSH` flat at
  ~115 s either side of the walker change).
- **Opportunity #4** (`HeaderWalker` pass 1) is landed. At europe
  scale it's +21 % wall (regression). At planet scale it's -9 %
  wall, because the `SORT_PASS2_END` page-cache-thrashing shutdown
  cost (32 788 majflt, 2.56 s) vanishes and pass 1 is fast enough
  that QD=1 latency doesn't dominate. Production scale = planet
  scale, so the walker is a net win in the scenario that matters.
- The europe regression stands but is a non-production-scale
  artifact. Not worth clawing back unless a separate europe-scale
  consumer surfaces.
- **Current wall is storage-stack-bound, not software-bound.**
  Planet post-walker buffered is 123.3 s, `--io-uring` is 118.1 s
  (-4 %). Both cap at ~800-830 MB/s because that's ext4's
  single-thread CFR ceiling on this NVMe (confirmed by the
  concurrency probe, 1.18x scaling at 2 threads). Opp #2 parks:
  parallel-writer chunking would buy ≤18 % wall (~21 s on planet)
  for days of code. On this ext4+NVMe setup the sort writer floor
  is structural; only opp #6 (reflink fast-path for zero-overlap
  sorted input on a reflink-capable output fs) offers further
  code-level wins.
- **io_uring is not a drop-in upgrade.** It buys 4 % wall at the
  cost of reintroducing page-cache thrashing on shutdown (the
  exact cleanup cost the post-walker buffered path eliminated).
  Keep the buffered path as default; use `--io-uring` only when
  the 4 % wall matters more than cleaner cache residency.

### Mitigations (parked - not justified by planet data)

Originally scoped to claw back the pass-1 wall regression on europe.
Planet data shows the walker is net positive at production scale,
so these are parked. Retained here for reference in case a
different consumer (europe-scale pipeline, cold-cache measurement
requirement) surfaces:

1. **Parallel walker (file-split + scan-forward).** Split the file
   into N byte-ranges. Each worker scans forward from its start to
   the next valid BlobHeader boundary (BlobHeader is self-delimiting:
   4-byte length prefix + parseable header type/size), then walks its
   region serially. Reassemble the per-worker `Vec<BlobEntry>`s in
   file-offset order at the end. Gives NVMe queue-depth parallelism;
   preserves `fadvise(RANDOM)` on each worker fd. No existing
   "sync to next BlobHeader" primitive in the tree - needs one. Days.
2. **io_uring batch-pread walker.** New primitive next to
   `HeaderWalker` that submits K header probes in flight, harvests
   completions, submits more. Recovers NVMe concurrency at the
   primitive level; single-threaded call-site; preserves
   `fadvise(RANDOM)`. Days; would also benefit `getid`,
   `apply_changes::scanner`, `inspect/scan.rs` if generalised.
3. **Streaming `posix_fadvise(DONTNEED)` on the BufReader path
   [TRIED 2026-04-24, REJECTED - zero-sum trade].** Implemented
   as `BufReader<File>` + `fadvise(SEQUENTIAL)` walk with
   `posix_fadvise(DONTNEED)` at 64 MB intervals, on top of commit
   `1f97fae`. Measured on europe (`53.8 s`, honest second run
   after cache drained): beat walker's 68.0 s by -21 %, matched
   pre-walker baseline 56.3 s. Cache-hygiene property worked:
   cleanup majflt 35 k → 0, no `SORT_PASS2_END` thrashing.
   Measured on planet: **136.3 s, +11 % regression vs walker**.
   M3 gives up the walker's pass-1 IO reduction (20 s BufReader
   vs 6.7 s walker on planet), which the europe win doesn't
   compensate for at the scale that matters. Summed across
   europe + planet, M3 total wall ≈ walker total wall; walker
   wins the tiebreaker because planet is production. First
   europe M3 run logged 45.4 s - cache-warm artifact from
   earlier session runs, not the honest number.
4. **`HeaderWalker` with `fadvise(SEQUENTIAL)` on the fd.** Opt-in
   flag on the walker primitive to use SEQUENTIAL instead of RANDOM.
   Restores async readahead pipelining for the monotonic
   walk - but pulls payload pages into cache too, so it's
   essentially BufReader semantics with a different code path.
   Collapses back to the pre-walker behavior; not a distinct win.

If the europe regression later matters: #1 or #2 recover wall
without giving up the planet walker win. #3 proven not worth it
(zero-sum trade). #4 is a non-distinct variant of pre-walker.

Recent instrumentation: commit `4e3c7ea` (2026-04-22) added phase
markers (`SORT_INDEX_BUILD`, `SORT_OVERLAP_DETECT`,
`SORT_WRITER_SETUP`, `SORT_WRITE_LOOP`, `SORT_FLUSH`) plus
counters + `#[hotpath::measure]`. That's instrumentation, not
architecture.

## Opportunities

Ranked with the sorted-input production path as the priority lens.

### 1. `copy_file_range` coalescing for passthrough runs [LANDED 244c6ec]

Transplanted the `apply-changes` drain coalescer (drain.rs:408-410,
587-597) into sort's pass 2 write loop: track an in-flight
`(start, end)` range, extend on contiguous-in-input blobs, flush as
one `write_raw_copy` on break (overlap run, missing-indexdata
fallback, end of loop). On already-sorted input the entire file
collapses into a single run.

Measured on europe (`740ed14f`): `sort_copy_range_calls = 1`,
`sort_copy_range_coalesced = 522 167`, `writer_pipeline_send_wait`
35 s → 2.65 µs. Did not move wall (53.0 s → 56.3 s, single-sample)
because the writer thread was already drain-limited by ext4
in-kernel CFR bandwidth - see "Takeaway" and "NVMe target
correction" above. Remains useful for any future change that
unpins the writer (e.g. parallel-writer chunking, opportunity #2).

### 2. Writer-side throughput on already-sorted input [PARKED - storage-stack bound]

With the producer doing one syscall, the writer is doing all the
wall. At planet scale that's 116 s of in-kernel ext4 CFR on NVMe
at ~800 MB/s (buffered) or 111 s of uring ReadFixed+WriteFixed at
~830 MB/s - both **single-thread-bound** (see "io_uring anatomy"
above). Measured 2026-04-24 that this ceiling is not a pbfhogg
architecture problem but an ext4+NVMe storage-stack limit; see
"ext4 CFR concurrency probe" below. All three sub-bullets resolved
as parked or downgraded:

- **Parallel writer on large CFR [PARKED - probe says 1.18x max]**:
  `probe_ext4_cfr.py` (deleted after use) ran 1 vs 2 concurrent
  `os.copy_file_range` into different output files on the same
  NVMe ext4 partition. Single thread: 792 MB/s (matches sort's
  measured 800 MB/s). Two concurrent threads: 931 MB/s aggregate
  (466 MB/s per thread). 1.18x scaling - ~18 % theoretical ceiling
  on planet, or ~21 s wall savings. The probe was optimistic
  (different inodes = less contention than same-file at different
  offsets), so real-world scaling would be worse. Days of code
  for <18 % wall on planet: not worth it. Something below the
  syscall layer (ext4 journal, block layer queue, NVMe controller)
  serialises. See "ext4 CFR concurrency probe" below.
- **io_uring passthrough [measured -4 %, not a default swap]**:
  `--io-uring --bench 1` on planet post-walker (`7f6288c0`) is
  118.1 s vs 123.3 s buffered. 4 % faster, but reintroduces
  page-cache thrashing on cleanup (25 k majflt, 1.65 s). The
  buffered path is still the cleanest default. See "io_uring
  anatomy" for full numbers. Not worth further work on this
  bullet in isolation.
- **Reflink-capable fs (btrfs, xfs with `reflink=1`)**: `copy_file_range`
  becomes an O(1) metadata op. Pass 2 drops from 116 s to <1 s. Not
  a code change - fs migration, out of scope for sort-level
  optimisation but worth noting as the theoretical ceiling.

Net: on ext4+NVMe the planet sort writer-side wall floor is ~115 s.
Further wins on this storage stack are not available at the
`parallel_writer.rs` layer. See opportunity #6 for a code-level
path that sidesteps the copy entirely on reflink-capable output
filesystems.

### 3. Parallel overlap-rewrite in pass 2 [LANDED 2026-04-24]

Overlap runs used to be processed sequentially: per-run
decompress → binary heap merge → re-encode, one run at a time,
blocking the main pass-2 write loop. Each run is self-contained
within one element type, so runs parallelise cleanly. The pattern
copies `altw/passthrough.rs` - produce `Vec<OwnedBlock>` in a
rayon worker, hand back to the main writer thread.

**Implementation:**
- `collect_overlap_runs(entries, overlaps)` -> `Vec<(start, end, kind)>`
  enumerates kind-bounded overlap spans upfront.
- `compute_overlap_run_local(entries, kind, input_path)` runs on a
  rayon worker: opens its own input fd, runs `sweep_merge_local`,
  emits `Vec<OwnedBlock>` + per-kind counts.
- Main sort() invokes `overlap_runs.par_iter().map(...).collect()`
  *before* the pass-2 write loop, buffering all overlap outputs in
  memory (marker pair `SORT_OVERLAP_PARALLEL_START/END`).
- Main loop drains the outputs in order via `writer.write_primitive_block_owned`,
  interleaved with passthrough CFR runs.
- Zero-cost on already-sorted input (the `overlap_runs.is_empty()`
  check short-circuits the par_iter entirely) - planet and already-
  sorted europe unchanged.
- Local writer helpers `write_single_{node,way,relation}_local`
  added to `src/owned.rs` mirroring the existing `write_single_*`
  but targeting `Vec<OwnedBlock>` via the
  `ensure_*_capacity_local` / `flush_local` pattern.

**Memory:** buffered output for all overlap runs accumulates in
memory before the serial write loop drains them. For typical
"mostly sorted" input (few small runs) this is small; for
pathologically unsorted input it can approach the input size. No
cap today - if this bites in the wild, the fix is a bounded rayon
pipeline with reorder buffer instead of collect().

**Correctness:** existing overlap tests (`sort_overlapping_blobs`,
`sort_overlap_rewrite_normalizes_dense_node_changeset_minus_one`,
`sort_overlap_runs_scoped_to_single_kind`, plus direct-io and
uring variants) all pass. Kind boundaries are preserved:
`collect_overlap_runs` splits at element-type transitions, same
invariant the old serial path had.

**Perf:** no planet-scale benchmark exists (production input is
already sorted, zero overlap runs). Predicted 1.5-3x on overlap
work based on rayon pool size vs serial baseline; unverified until
an unsorted dataset is added to `brokkr.toml`.

### 4. `HeaderWalker`-based pass 1 [LANDED 1f97fae, planet net positive]

Migrated pass 1 from `FileReader` (BufReader + `fadvise(SEQUENTIAL)`)
to `HeaderWalker` (pread + `fadvise(RANDOM)`). Mirrors
`inspect/scan.rs::try_index_only_scan`. `direct_io` now ignored
inside pass 1 (walker owns the fd; non-indexed fallback uses
`walker.pread_data`).

Europe (`25d71ce7`): pass 1 disk read 2.86 GB (from ~34 GB, **-91 %**),
pass 1 wall 27.4 s (from 16-18 s, **+49 %**), total wall 68.0 s
(from 56.3 s, **+21 %**) - regression.

Planet (`bb392a17` vs `1aef9d9c`): pass 1 disk read 0.67 GB (from
32 GB, **-98 %**), pass 1 wall 6.73 s (from 18.94 s, **-65 %**),
total wall 123.3 s (from 135.1 s, **-9 %**) - win. `SORT_PASS2_END`
cleanup (2.56 s + 32 788 majflt from page-cache thrashing)
vanishes completely.

Planet is the production scale, so the migration is a net positive
in the scenario that matters. See "Mitigations" above for notes on
clawing back the europe regression if a europe-scale consumer ever
surfaces; currently parked.

### 5. Frame buffer hoisting (micro) [SUBSUMED BY #3 2026-04-24]

The original concern: `sweep_merge` allocated a fresh `Vec<u8>`
per overlap run. The #3 refactor moved the sweep into worker-local
`sweep_merge_local`, where `frame_buf` is a local `Vec::new()`
once per worker invocation = once per run. `Vec::resize` reuses
capacity across entries within a run, so within-run reuse is
already in place.

Across-run reuse (rayon thread-local state via `map_init`) is a
further ~0 % improvement (one Vec allocation per run is rounding
error next to the I/O and decode work). Not implemented.

### 6. Reflink fast-path for zero-overlap sorted input

When pass 1 finds the input is already sorted with zero overlap
runs (the production case) and the output filesystem supports
reflinks (btrfs, xfs with `reflink=1`), skip pass 2's per-byte
copy and issue a single `FICLONE` / `FICLONERANGE` ioctl instead.
On reflink-capable fs this completes in O(1) metadata time
regardless of file size - planet sort would drop from 123 s
(post-walker) to ~7 s (pass 1 only) with no bytes physically
copied.

The fs check is `copy_file_range` behaviour: on reflink-capable
fs it already collapses to metadata; on ext4 it does a real
in-kernel copy. Alternatively, `FICLONE` ioctl returns EOPNOTSUPP
on non-reflink fs and can be the explicit probe.

Semantics: sort's contract today is "verify-and-reframe" -
output bytes may differ from input even when ordering is
preserved (e.g. re-compressed blobs, header rewrite with
`sorted=true` hint). A reflink fast-path would skip reframing
entirely when input is already optimally ordered, which is a
semantic change worth flagging:
- The `Sort.Type_then_ID` header hint might not be set on the
  input even when the data is sorted - refusing to reflink in
  that case keeps the existing behaviour. Or reflink + patch the
  header in place via a tiny write to update the sort hint.
- Callers expecting a freshly-framed output (e.g. downstream
  tools that depend on specific compression parameters) would
  get the input's framing instead.

Conditional fast-path (opt-in flag or auto-detect + opt-out)
keeps the default safe. Days scope: reflink syscall plumbing,
header-hint patching, conditional dispatch, and an
integration test covering both "reflink taken" and "reflink
declined" paths. Largest remaining opportunity for the
already-sorted production path on a reflink-capable fs.

## Correctness finding: intra-blob disorder is invisible (2026-07-10)

Blob-level permutation sort assumes every blob is internally sorted
and nothing checks it. Given a file whose blobs are internally
UNSORTED but whose blob (min_id, max_id) ranges do not overlap, sort
emits a byte-identical copy of the input stamped `Sort.Type_then_ID` -
silent corruption of the sorted invariant. Found via
`degrade --unsort` + `verify sort --snapshot unsorted`: degrade's swap
accidentally landed intra-blob (see `notes/degrade.md` "Known bugs"),
`detect_overlaps` correctly saw no range overlap (run `f5cd6522`:
0 overlaps, 7,399 passthrough, one whole-file `copy_file_range`), and
osmium's element-level sort fixed what we passed through. The
overlap-rewrite path itself has therefore STILL never been exercised
on real unsorted data.

**Resolved (2026-07-11).** Recorded in CORRECTNESS.md ("`sort`: intra-blob
disorder"). An external review falsified the first ruling's premise that
"indexdata implies pbfhogg-sorted payload" - `cat` attaches indexdata to
third-party blobs without reordering them, and
`PbfWriter::write_primitive_block` indexes caller-provided blocks as-is, so
unsorted non-indexed -> `cat` -> `sort` yielded a false sorted claim. The
final ruling keys trust on the header claim instead:

- **Checked scan wherever the payload is decoded.** `scan_block_ids` grew a
  checked twin, `scan_block_ids_checked`, that tracks intra-blob
  monotonicity (canonical OSM order via `osm_id_cmp`) during the scan
  that already computes min/max - one compare per element. `build_blob_index`
  flags any out-of-order blob, and `sort` folds that flag into
  the overlap set so pass 2 decodes + re-encodes it (handled, not errored -
  sorting is the command's job). `sort` prints a
  `blobs internally unsorted (decode + re-encode)` line and emits a
  `sort_blobs_intra_unsorted` counter when it fires.
- **Trust is keyed on the header's `Sort.Type_then_ID` claim, not on
  indexdata presence.** Declared-sorted indexed input: header-only pass 1,
  passthrough design intact. Indexed input WITHOUT the sorted claim (the
  `cat`-over-unsorted shape, and `degrade --unsort*` output, which clears
  the claim): pass 1 preads + decompresses + checked-scans every payload,
  with a one-line stderr notice, and uses the fresh scan for range
  analysis. This is the same trust boundary `ElementReader` already uses.
- **Residual, documented:** a header that claims sorted over internally
  unsorted blobs violates its own contract and passes through undetected.
  No `--verify-blobs` flag - wanting verification means arriving without
  the claim, which now gets it by default. Producers (`cat`, `PbfWriter`)
  deliberately unchanged: indexdata range info is valid for unsorted blobs
  and every other consumer only uses it for range queries.

The overlap-rewrite path is now also exercised on real unsorted data by
`degrade --unsort` (cross-blob) and `degrade --unsort-intra --strip-indexdata`
(intra-blob), closing the "STILL never been exercised" gap noted above.

## Things that deliberately do not change

- **Pipelined decode is not adopted.** `sort` uses direct pread per
  blob (`reference/pipelined-reader-paths.md:138`); the decode
  pattern is correct for the two-pass shape and the anti-conversion
  rule applies.
- **io_uring writer is already integrated** and used by the write
  path when `--io-uring` is passed; the coalescer (opportunity #1)
  operates *inside* that path, not alongside it.
- **Sort is not a production-pipeline command.** It exists to fix
  unsorted PBFs from third-party tools; pbfhogg's own commands
  preserve order. Optimisation priority follows: anything that
  helps the already-sorted case first, unsorted-case optimisations
  only after a benchmark exists.

## Prerequisites before shipping anything

1. **Europe runs landed** (`043cf4b6` pre-coalesce, `740ed14f`
   post-coalesce, `25d71ce7` post-walker).
2. **Planet runs landed** (`1aef9d9c` pre-walker 135.1 s,
   `bb392a17` post-walker 123.3 s). Walker confirmed net positive
   at production scale; europe regression confirmed a
   non-production-scale artifact.
3. **NVMe→NVMe measurement** obtained - the existing europe and
   planet benches already run NVMe→NVMe on the same ext4 partition
   (see "NVMe target correction"). No separate bench needed; the
   800 MB/s in-kernel ext4 CFR ceiling is what the writer sees.
4. **Planet `--io-uring` bench landed** (`7f6288c0`, 118.1 s, -4 %
   vs buffered). See "io_uring anatomy" - marginal win, not worth
   defaulting to.
5. **Planet hotpath landed** (`e42b0c8c`, 119.3 s). Confirms main
   thread blocked on writer; annotated frames cover only 4 % of
   wall. See "Hotpath anatomy".
6. **Cold-cache planet bench (`drop_caches` on `1f97fae`)** to close
   out the warm-cache caveat on the -65 % pass-1 wall win. Parked
   as future work, not blocking.
7. **ext4 CFR concurrency probe landed** (`probe_ext4_cfr.py`,
   deleted after use). 1 vs 2 concurrent `os.copy_file_range` on
   planet: 792 MB/s → 931 MB/s aggregate, 1.18x scaling.
   Closes opp #2 as not worth the days. See "ext4 CFR concurrency
   probe" above.
8. **Reflink-capable output fs probe** (for opp #6): verify
   `FICLONE`/`FICLONERANGE` behaviour on btrfs and xfs-with-reflink
   to size the fast-path. Prereq for shipping opp #6; not landed.
9. **Unsorted-input dataset** for opportunity #3. None in
   `brokkr.toml` today; would need configuring (or a synthetic
   fixture) before that opportunity can be sized.

## Cross-references

- [`reference/pipeline.md`](../reference/pipeline.md) - "sort" entry
  under Command Pipelines; also "sort is not in the pipeline"
  discussion at line 231.
- [`reference/pipelined-reader-paths.md`](../reference/pipelined-reader-paths.md) -
  line 138, "sort uses direct pread per blob" rationale.
- [`reference/performance.md`](../reference/performance.md) -
  Denmark (line 774) and Japan (line 808) already-sorted baselines,
  Denmark osmium comparison (line 863) showing 83x win for the
  sorted/indexed case.
- [`src/commands/sort/mod.rs`](../src/commands/sort/mod.rs) - entry
  point; the write loop and sweep_merge live here.
- [`src/commands/apply_changes/drain.rs`](../src/commands/apply_changes/drain.rs)
  and
  [`src/commands/apply_changes/streaming.rs`](../src/commands/apply_changes/streaming.rs) -
  the `copy_file_range` coalescer pattern to transplant for
  opportunity #1.
- [`src/read/header_walker.rs`](../src/read/header_walker.rs) - the
  HeaderWalker primitive for opportunity #3.
