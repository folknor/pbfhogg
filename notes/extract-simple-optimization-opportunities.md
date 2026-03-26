# `extract --simple` optimization opportunities

## Scope

This note covers the `ExtractStrategy::Simple` path in:

- `src/commands/extract.rs`

Current TODO reference:

- `TODO.md` -> "Extract simple: remaining gap vs osmium"

## Problem statement

Current `extract --simple` is slower than osmium (from TODO context):

- ~1.47x slower on Denmark
- ~1.70x slower on Japan

Main stated reason: current implementation is two-pass over the input.

## Current implementation (from code)

`extract_simple` currently does:

1. **Pass 1 (full read):**
   - build `bbox_node_ids`
   - build `matched_way_ids` (way has ref in `bbox_node_ids`)
   - build `matched_relation_ids` (relation has member in matched node/way sets)

2. **Pass 2 (full read again):**
   - parallel batch rewrite using `process_extract_pass2_batch`
   - writes:
     - nodes in bbox
     - ways referencing bbox nodes
     - relations referencing matched nodes/ways

For simple mode, `all_way_node_ids` is intentionally empty, so it does **not** pull extra way dependency nodes.

## Key insight: single-pass is feasible on sorted inputs

The command writes output as sorted (`.sorted()` in header builder), and normal inputs are typically sorted by type then ID.

For sorted inputs (`nodes -> ways -> relations`):

- by the time we process ways, all nodes are already known
- by the time we process relations, all matched ways are already known

That means the pass-1 dependency sets needed for decisions are naturally available during one forward scan, without revisiting the file.

This is the strongest near-term path to remove the extra file read cost.

## High-confidence optimization opportunities

## 1) Single-pass simple extraction for sorted input [DONE]

Design:

- stream blocks once (`into_blocks_pipelined`)
- maintain mutable sets:
  - `bbox_node_ids`
  - `matched_way_ids`
- classify and emit in the same pass:
  - nodes: evaluate bbox, emit if in bbox, record ID
  - ways: evaluate refs against `bbox_node_ids`, emit + record way ID when matched
  - relations: evaluate members against `bbox_node_ids` and `matched_way_ids`, emit when matched

Expected impact:

- removes second file read entirely
- should close most of the current gap vs osmium for simple mode

## 2) Keep two-pass fallback for unsorted input [DONE]

Single-pass above relies on type ordering.

Recommended behavior:

- if `header().is_sorted()` -> single-pass fast path
- else -> current two-pass implementation (correctness-preserving fallback)

## 3) Reuse existing parallel batch machinery for write side [DONE]

`process_extract_pass2_batch` already performs parallel block rewriting and stats merge.
Single-pass variant can reuse most of this code by:

- using streaming classification + per-batch processing
- avoiding full precomputed ID sets where unnecessary

## 4) Conditional blob filtering for pass-1/two-pass fallback [DONE]

Added `spatial_blob_filter(&bbox_int)` to both the unsorted two-pass fallback
and the sorted single-pass reader. Skips decompression of node blobs whose
coordinate bbox doesn't intersect the extract region (requires v2 indexdata;
raw PBFs pass all blobs through conservatively).

## Medium-confidence opportunities

## 5) Type-phase aware fast path (no per-way `any()` in wrong phases) [DONE]

`classify_block_simple` now branches on `block.block_type()` — DenseNodes|Nodes,
Ways, Relations, Mixed|Empty — eliminating dead match arms in sorted PBF inner
loops. Mixed/Empty falls through to the original match-all logic for correctness.

## 6) Skip empty blocks in write path [DONE]

`classify_block_simple` returns `bool` indicating whether any elements matched.
Single-pass path skips `batch.push(block)` when no matches, avoiding dispatch
to `process_extract_pass2_batch` for blocks that would produce zero output.
Big win for sparse extracts (small bbox on large file).

## 7) Stats write-path micro-optimizations [SKIPPED]

Too minor to justify the code complexity. Type-phase branching (#5) already
eliminates most of the redundant work these would address.

## Risks and constraints

## 1) Sortedness assumption and header correctness

Current extract paths mark output header as sorted unconditionally (`.sorted()`), but no explicit sorted-input check is performed.
Single-pass fast path should make this contract explicit.

Recommended:

- for single-pass fast path: require `header().is_sorted()`
- for unsorted fallback: either preserve old behavior or avoid claiming sorted output

## 2) Semantic compatibility

Must preserve simple semantics exactly:

- include only bbox nodes
- include ways referencing bbox nodes
- include relations with matched member node/way
- do **not** include extra way dependency nodes

## 3) Parallelism contention

Prior notes show consumer-side rayon contention risks with pipeline decode pool.
Any new parallel classification path should avoid introducing a second competing parallel stage in the same pass unless measured.

## 4) Sparse vs dense region behavior

Performance improvements may vary by extract selectivity.
Single-pass removes I/O regardless, but CPU behavior may still vary significantly by region density.

## Theoretical / uncertain opportunities — ALL CLOSED

Reviewed by perf + arch teams. None address the remaining osmium gap,
which is structural: double element iteration (classify then re-encode),
re-encoding overhead (full decode+rebuild vs osmium's raw passthrough),
and zlib-rs vs C zlib (15-19% sync compression gap).

## ~~A) Deferred relation matching~~ — CLOSED

0% impact on osmium gap. Only helps unsorted input, which doesn't exist
in production (all Geofabrik/planet PBFs are Sort.Type_then_ID). The
sorted single-pass path (item 1) already handles all production inputs.

## ~~B) Block-level relation prefilter~~ — CLOSED

<1% impact. Relation blobs are already cheap (Denmark: ~35 blobs, Japan:
~100 blobs). Index format evolution cost is high. Member-type summaries
have low selectivity (relations span wide ID ranges).

## ~~C) Unified extract engine~~ — CLOSED (full refactor)

0% perf impact — maintainability only. Current code already has the right
unification (collect_pass1_generic + RelationHandler for pass 1, shared
process_extract_pass2_batch for write path). Remaining duplication is
~150 lines of structurally similar but semantically distinct code.

A full unified pass planner would add abstraction complexity and coupling
risk without removing code. The fused single-pass path (simple sorted)
is fundamentally different from multi-pass (complete/smart).

**One low-cost improvement identified:** merge extract_block_pass2 and
extract_block_pass3 into a single function with a unified filter struct
(ExtractPass2IdSets + ExtractPass3IdSets → one struct with optional
extra_way_ids/extra_node_ids). Eliminates ~100 lines, low risk.

## ~~D) Adaptive batch sizing~~ — CLOSED

<0.5% impact. Write path is not the bottleneck. Item 6 (skip empty
blocks) already handles sparse extracts. For dense extracts, batches
stay full at 64 and adaptive sizing has no effect.

## Implementation status

All practical items (1-6) done, #7 skipped. Theoretical items A-D all
closed per reviewer consensus. Verified via `brokkr verify extract`
(all 3 strategies pass) and `brokkr check` (clippy + tests clean).

Initial single-pass results (items 1-3): Denmark -14% (2625→2277ms),
Japan -8% (12619→11948ms). Items 4-6 not yet benchmarked separately.
