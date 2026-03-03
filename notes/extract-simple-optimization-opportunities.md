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

## 1) Single-pass simple extraction for sorted input

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

## 2) Keep two-pass fallback for unsorted input

Single-pass above relies on type ordering.

Recommended behavior:

- if `header().is_sorted()` -> single-pass fast path
- else -> current two-pass implementation (correctness-preserving fallback)

## 3) Reuse existing parallel batch machinery for write side

`process_extract_pass2_batch` already performs parallel block rewriting and stats merge.
Single-pass variant can reuse most of this code by:

- using streaming classification + per-batch processing
- avoiding full precomputed ID sets where unnecessary

## 4) Conditional blob filtering for pass-1/two-pass fallback

Current simple pass-1 reads all blobs without `with_blob_filter`.
When indexdata is present, a spatial node filter and/or type filter can reduce decode work.

Practical option:

- if indexdata is available: use `spatial_blob_filter` for node-heavy parts
- if not: keep conservative full decode behavior

## Medium-confidence opportunities

## 5) Type-phase aware fast path (no per-way `any()` in wrong phases)

Given sorted input, branch by phase:

- node phase: only node logic
- way phase: only way logic
- relation phase: only relation logic

This trims branching and redundant checks in hot loops.

## 6) Borrowed-element classification before owned rewrite

Current parallel write path materializes owned blocks. For simple mode, consider:

- cheap match precheck on borrowed elements
- only allocate owned output buffers for blocks with matches

Potential win on sparse extracts where many blocks produce no output.

## 7) Stats write-path micro-optimizations

Minor but cheap:

- avoid repeated `region.contains_decimicro` conversions in bbox mode by pre-resolving strategy function
- hoist reusable buffers aggressively in single-pass block processors

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

## Theoretical / uncertain opportunities

## A) Deferred relation matching with lightweight member bloom/filter

For unsorted inputs, buffer unresolved relations cheaply (ID + compact member refs) until prerequisite sets are known.

- could reduce need for full second pass in partially unsorted data
- complexity and memory-risk uncertain

## B) Block-level relation prefilter from tag/index metadata

Extend blob metadata with relation member-type summaries to skip relation block decoding in simple mode when impossible to match.

- potentially useful but requires index format evolution and writer overhead
- uncertain ROI

## C) Unified extract engine with strategy-specific policies

Build a common pass planner (simple/complete/smart) that chooses single/multi-pass automatically based on sortedness + index richness.

- good long-term maintainability
- higher up-front refactor cost

## D) Adaptive batch sizing by match density

Dynamically shrink/grow parallel batch size based on recent output density to balance latency vs throughput.

- possible micro-gain
- uncertain net benefit vs complexity

## Suggested implementation order

1. Add explicit sortedness branch in `extract_simple`.
2. Implement single-pass fast path for sorted input (no semantic changes).
3. Keep existing two-pass path for unsorted fallback.
4. Add conditional blob filtering in fallback when indexdata is available.
5. Benchmark Denmark/Japan and compare with current baseline.

## Measurement plan

Run sequentially (one benchmark at a time):

- `extract --simple` on Denmark and Japan
- sorted input and unsorted synthetic variant

Track:

- wall time
- peak RSS
- decoded blob count (if instrumented)
- output parity vs current implementation (byte-equal or element-equal checks)

Success criteria:

- clear wall-time drop on sorted inputs (primary path)
- no behavior regressions
- fallback correctness on unsorted inputs
