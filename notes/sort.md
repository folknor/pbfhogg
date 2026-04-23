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

## Current state (2026-04-23)

Europe runs landed today (host `plantasjen`, `target=hdd` output per
`brokkr env`):

| mode    | uuid       | commit    | wall    | notes |
|---------|------------|-----------|---------|-------|
| bench   | `043cf4b6` | `b891514` | 53.0 s  | baseline, pre-coalesce |
| alloc   | `99c58e53` | `b891514` | 54.7 s  | alloc-instrumented |
| hotpath | `fd2ef4e7` | `b891514` | 64.7 s  | hotpath-instrumented |
| bench   | `740ed14f` | `244c6ec` | 56.3 s  | **post-coalesce**, `--bench 1` |

Planet baseline still pending; `overnight.sh:272-275` runs
`brokkr sort --dataset planet --bench 1`, `--io-uring --bench 1`,
`--hotpath`, `--alloc` tonight on `244c6ec`, lands morning of
2026-04-24. Denmark (indexed, sorted) 366 ms, Japan (indexed, sorted)
1.33 s,
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
work shorter. On the HDD-EXDEV fallback path (`copy_range_fallback`,
256 KB pread+write), the writer's throughput ceiling is sequential
HDD bandwidth, not syscall overhead.

Peak RSS also moved +100 MB (805 → 911 MB pass 1; 817 → 913 MB pass
2) with no obvious code reason. Most likely allocator watermark under
a different request pattern - worth watching on planet, not worth
chasing on europe.

### Takeaway

Opportunity #1 is **mechanically correct and landed**: the producer
is now O(runs) syscalls instead of O(blobs), and the accounting
(vol_cs, majflt at shutdown, send-wait) reflects that cleanly. The
wall-time thesis ("syscalls are the bottleneck") was wrong for this
target: the writer was already drain-limited. The new lever for
wall-time work is the writer side, not the producer side. Planet
tonight (on `244c6ec`) will settle whether coalescing pays off once
the target filesystem is NVMe rather than HDD-EXDEV.

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
because the writer thread was already drain-limited - see "Takeaway"
above. Remains potentially useful on NVMe-target production
(no EXDEV fallback) and for any future change that unpins the
writer.

### 2. Writer-side throughput on already-sorted input [new top priority]

With the producer now doing one syscall, the writer is doing all the
wall. Options worth sizing:

- **Parallel writer on large CFR**: today `parallel_writer.rs`
  round-robins `OutputChunk::CopyRange` ops to workers, so one giant
  coalesced op goes to a single worker (see
  `parallel_writer.rs:276-289`). Pre-coalesce, 522 k ops
  round-robined across the pool. Chunking a coalesced run back into
  N pieces at dispatch time would restore pipelining. For the HDD
  target this may or may not help (seek contention); for NVMe it
  should.
- **io_uring passthrough**: the uring writer's `handle_copy_range_uring`
  already chunks into 256 KB WriteFixed ops (`uring_writer.rs:416`).
  Benchmark pending whether the io_uring variant beats the buffered
  variant on an already-sorted planet.
- **Target filesystem**: benches hit HDD scratch (`target=hdd`).
  Real production targets NVMe; `copy_file_range` stays in-kernel
  (reflink on btrfs/xfs, copy-offload elsewhere) rather than falling
  through to userspace pread+write. A single NVMe→NVMe bench on
  europe would clarify how much of the baseline 53 s is EXDEV tax
  vs. genuine work.

Hours-to-days scope depending on how much writer-side restructuring
this wants. Size against planet overnight data (`2026-04-24`) before
picking a specific path.

### 3. Parallel overlap-rewrite in pass 2

Overlap runs are currently processed sequentially: per-run
decompress → binary heap merge → re-encode. Each overlap run is
self-contained within one element type, so runs parallelise cleanly
with rayon `par_iter` + a reorder buffer into the writer.

Estimated 1.5-3x on overlap runs. Only exercised by genuinely
unsorted input, so this does not move the production benchmark.
Worth picking up only if overnight data on unsorted-input scenarios
lands (no such dataset configured in `brokkr.toml` today - new
dataset or injection would be a prerequisite).

Hours scope once the write path accepts out-of-order completions.

### 4. `HeaderWalker`-based pass 1 on non-indexed input

Pass 1 currently decompresses non-indexed blobs to recover their
`(kind, min_id, max_id)`.
[`HeaderWalker`](../src/read/header_walker.rs) + pread workers do the
same with header reads only, falling back to decompress only where
indexdata is missing.

Estimated 1.2-2x pass 1 time on non-indexed input. Indexed input
already skips the decompress via the existing fast path, so this
only helps inputs without indexdata, which the production pipeline
never has.

Days scope - adapting the HeaderWalker + pread pattern from
`getid` / `apply-changes::scanner`. Deprioritise until an
unsorted-input benchmark exists to size the win.

### 5. Frame buffer hoisting (micro)

`sweep_merge` allocates a fresh `Vec<u8>` per overlap run. Hoist to
the outer write loop and reuse. Estimated <1 % wall; matters only
if profile shows allocator pressure, which is unlikely at the
current blob counts.

Minutes scope. Land only as part of a sweep of related changes.

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

1. **Europe baseline + post-coalesce run landed** (`043cf4b6` and
   `740ed14f`). Planet baseline still scheduled for
   `overnight.sh:272-275` tonight, lands 2026-04-24 on commit
   `244c6ec`. That will tell us whether coalescing pays off when the
   workload scales and whether the `target=hdd` quirk explains the
   europe wall-flat result.
2. **NVMe→NVMe europe bench** to isolate the EXDEV-fallback cost in
   the writer from the genuine copy work. Prereq for sizing
   opportunity #2 ("Writer-side throughput").
3. **Unsorted-input dataset** for opportunities #3 and #4. None in
   `brokkr.toml` today; would need configuring (or a synthetic
   fixture) before those opportunities can be sized.

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
