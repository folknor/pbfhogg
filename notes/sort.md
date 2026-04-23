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

Europe baseline landed today (commit `b891514`, host `plantasjen`):

| mode    | uuid       | wall    | notes |
|---------|------------|---------|-------|
| bench   | `043cf4b6` | 53.0 s  | `--bench 1`, buffered writer |
| alloc   | `99c58e53` | 54.7 s  | alloc-instrumented |
| hotpath | `fd2ef4e7` | 64.7 s  | hotpath-instrumented |

Planet baseline still pending; `overnight.sh:272-275` runs
`brokkr sort --dataset planet --bench 1`, `--io-uring --bench 1`,
`--hotpath`, `--alloc` tonight, lands morning of 2026-04-24. Denmark
(indexed, sorted) 366 ms, Japan (indexed, sorted) 1.33 s,
[`reference/performance.md:774`](../reference/performance.md#L774).

Europe `043cf4b6` phase split:

- `SORT_INDEX_BUILD` 16.39 s (30.9 %)
- `SORT_OVERLAP_DETECT` 30 ms
- `SORT_WRITE_LOOP` 35.01 s (66.1 %)
- `SORT_FLUSH` 905 ms

Counters confirm the already-sorted path: `sort_blobs_passthrough =
522168`, `sort_blobs_overlap = 0`, `sort_blobs_rewritten = 0`, 35.26
GB in = 35.26 GB out. The writer issues **522 168 single-blob
`copy_file_range` calls** (`writer_payload_copy_range_items`), one
per blob, with `writer_write_ns = 34.72 s` accounting for essentially
all of pass 2. `SORT_WRITE_LOOP` averages 0.3 cores with 519 k
voluntary context switches: syscall-bound, not CPU-bound.

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

Recent instrumentation: commit `4e3c7ea` (2026-04-22) added phase
markers (`SORT_INDEX_BUILD`, `SORT_OVERLAP_DETECT`,
`SORT_WRITER_SETUP`, `SORT_WRITE_LOOP`, `SORT_FLUSH`) plus
counters + `#[hotpath::measure]`. That's instrumentation, not
architecture.

## Opportunities

Ranked with the sorted-input production path as the priority lens.

### 1. `copy_file_range` coalescing for passthrough runs

Already-sorted input is ~100 % non-overlap passthrough blobs. Current
pass 2 issues one write per blob; with io_uring that's one SQE per
blob, with the buffered writer that's one `write_raw_copy` per blob.
Neither coalesces consecutive passthrough frames from the same input
file.

Europe `043cf4b6` quantifies the baseline: 522 168
`copy_file_range` calls for one contiguous 35 GB passthrough run.
`SORT_WRITE_LOOP` at 0.3 avg cores and 519 k voluntary context
switches is consistent with one context switch per `copy_file_range`
syscall.

`apply-changes`'s drain path already coalesces adjacent passthrough
frames into single `copy_file_range` spans (see `drain.rs` and
`streaming.rs` coalescer logic). Transplanting that pattern to sort's
write loop reduces syscall / SQE count by the run length, which on
already-sorted inputs is the entire file.

Sized against europe: `copy_file_range` caps at ~2 GB per call
(kernel-side chunking), so a single 35 GB run coalesces into ~18
calls rather than 522 168. Expected 1.3-2x on the already-sorted
europe wall via syscall reduction; planet's 85 GB becomes ~43 calls.
Directly benefits the production scenario.

Hours scope. No risk beyond matching the existing pattern's
boundary-flush discipline (flush on overlap run start, type change,
end of file).

### 2. Parallel overlap-rewrite in pass 2

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

### 3. `HeaderWalker`-based pass 1 on non-indexed input

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

### 4. Frame buffer hoisting (micro)

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
  path when `--io-uring` is passed; opportunity #1 operates *inside*
  that path, not alongside it.
- **Sort is not a production-pipeline command.** It exists to fix
  unsorted PBFs from third-party tools; pbfhogg's own commands
  preserve order. Optimisation priority follows: anything that
  helps the already-sorted case first, unsorted-case optimisations
  only after a benchmark exists.

## Prerequisites before shipping anything

1. **Europe baseline landed** (commit `b891514`, uuid `043cf4b6`,
   53.0 s). Planet baseline still scheduled for
   `overnight.sh:272-275` tonight, lands 2026-04-24. Europe alone is
   enough to exercise and measure opportunity #1; planet will
   confirm the scaling.
2. **Unsorted-input dataset** for opportunities #2 and #3. None in
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
