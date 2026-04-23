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

No `.brokkr/results.db` rows at planet scale yet.
`overnight.sh:272-275` runs `brokkr sort --dataset planet --bench 1`,
`--io-uring --bench 1`, `--hotpath`, `--alloc` tonight, so the
baseline lands morning of 2026-04-24. Denmark (indexed, sorted) 366 ms,
Japan (indexed, sorted) 1.33 s,
[`reference/performance.md:774`](../reference/performance.md#L774).

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

`apply-changes`'s drain path already coalesces adjacent passthrough
frames into single `copy_file_range` spans (see `drain.rs` and
`streaming.rs` coalescer logic). Transplanting that pattern to sort's
write loop reduces syscall / SQE count by the run length, which on
already-sorted inputs is the entire file.

Estimated 1.1-1.5x on the already-sorted planet case via syscall
reduction. Directly benefits the production scenario.

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

1. **Planet baseline** scheduled for `overnight.sh:272-275` tonight.
   Baseline lands 2026-04-24. That's already-sorted planet; covers
   the production path and opportunity #1.
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
