# Streaming merge-join plan for `derive-changes` and `diff`

## Status

**Phase 1 (`diff`) complete** — commit `b14a174`.

- `diff` rewritten to streaming merge-join over pipelined block iterators.
- New `src/commands/stream_merge.rs` module: `StreamingBlocks` cursor with
  block-level stashing, `fill_buffer`/`next_element` typed extraction,
  per-type conversion functions.
- Requires `Sort.Type_then_ID` on both inputs; actionable error if missing.
- Denmark (465 MB, 59.1M elements): 14.65s wall, ~1.1 GB RSS (bounded by
  pipeline decode buffers, not element count).
- 16 integration tests (12 migrated + 4 new: unsorted rejection, empty files,
  multi-block boundary 9000 vs 7500, type_filter=way phase skipping).

**Phase 2 (`derive_changes`) complete.**

- Ported to same `StreamingBlocks` cursor from `stream_merge.rs`.
- `MergeElement` trait + `classify`/`streaming_merge_phase` generic loop.
- Changes buffered by action type (`Changes` struct) for grouped OSC output.
- Memory bounded by number of changed elements, not total input size.
- Requires `Sort.Type_then_ID` on both inputs (errors with guidance to sort).
- 11 integration tests (10 existing migrated to sorted PBFs + 1 new
  unsorted rejection test).

**Phase 3 (cleanup) complete.**

- Removed `read_elements()`, `ReadResult`, and `take_*` clone helpers
  from `owned_elements.rs` (no remaining callers).
- Kept: owned type definitions, equality functions, coordinate helpers
  (used by both `diff` and `derive_changes` via `stream_merge.rs`).

## Context

This note replaces the long `TODO.md` discussion for:

- **P3-22: Streaming merge-join for `derive_changes` / `diff`**

Current implementations in:

- `src/commands/derive_changes.rs`
- `src/commands/diff.rs`
- shared materialization helpers in `src/commands/owned_elements.rs`

Both commands currently:

1. load both input PBFs fully into owned vectors (`read_elements`)
2. sort nodes/ways/relations by ID
3. run per-type two-pointer merge-join

This is correct for small/medium extracts but is a hard OOM path at large scale.

## Current behavior and constraints (from code)

## `derive_changes`

- Reads both files via `read_elements(old/new, direct_io, None)`.
- Sorts 6 vectors (`old/new` x nodes/ways/relations).
- Merge-joins by ID and classifies into `create`, `modify`, `delete`.
- Writes OSC XML grouped by action (`<create>`, `<modify>`, `<delete>`), and for each action grouped by type (nodes, ways, relations).
- Comparison helpers (`nodes_equal`, `ways_equal`, `relations_equal`) intentionally do **not** consider version-only changes as modifications (existing semantics).

## `diff`

- Same materialize + sort pattern.
- Optional `BlobFilter` prefilter by element type before decode.
- Merge-joins and writes text output incrementally.
- `verbose` mode needs both old and new element values at ID match time (naturally available in merge-join).
- Same equality semantics as above.

## Shared helpers

- `owned_elements.rs` owns the current all-in-memory path and type definitions.
- That module is only used by `derive_changes` and `diff` right now.

## Why streaming is feasible

`ElementReader` already provides:

- `header()` with `HeaderBlock::is_sorted()`
- `into_blocks_pipelined()` returning `Iterator<Item = Result<PrimitiveBlock>>`

When files are `Sort.Type_then_ID`:

- blocks are effectively type-runs (nodes, then ways, then relations)
- elements inside each block are ordered by ID

So a two-file streaming merge-join can keep only:

- current block/cursor for old
- current block/cursor for new

Memory becomes bounded (roughly O(current blocks)), rather than O(total elements).

## Target architecture

## 1) Streaming element cursor

Introduce a small cursor abstraction (shared by both commands), e.g.:

- input: `PipelinedBlocks` from one file
- output: next owned comparable element in sorted order, per type
- handles block boundaries internally

Recommended shape:

- one cursor per type (`NodeCursor`, `WayCursor`, `RelationCursor`) or
- one generic cursor with explicit type-phase transitions

Per-type cursors are simpler to reason about and test.

## 2) Type-phase merge driver

Drive merge in fixed phases:

1. nodes
2. ways
3. relations

For each phase, run existing two-pointer logic but against streaming cursors instead of slices.

## 3) Sorted-input contract

Before streaming merge:

- open both readers
- verify `old.header().is_sorted()` and `new.header().is_sorted()`
- return clear error if either is unsorted

This removes in-memory sort, but changes behavior for unsorted files.

### Unsorted input fallback options

1. **Strict mode only (recommended initial):** error with guidance to run `pbfhogg sort` first.
2. External-sort fallback inside command (more complexity, out of scope for first pass).

## 4) Output strategy differences by command

## `diff` output

Easy to stream end-to-end:

- emit lines immediately as merge advances
- no global buffering required
- `verbose` mode works naturally for equal-ID comparisons

## `derive_changes` output

Main complication is OSC action grouping.

Streaming merge order is by type+ID, but output requires grouped actions.

Practical options:

1. **Buffer changed elements by action in memory** (recommended first pass).
2. Write action streams to temp files, then concatenate.
3. Multi-pass join (3 passes, one per action), avoids buffering but triples I/O.

Option 1 is simplest and keeps memory bounded by number of changed elements, not by total input size.

## Implementation plan

## Phase 1: `diff` first

1. Add sorted-header checks.
2. Build streaming cursor utility.
3. Reuse existing pairwise compare/emit functions with minimal changes.
4. Preserve exact CLI output format and stats semantics.

Expected result: remove OOM risk for `diff` with minimal behavior change.

## Phase 2: `derive_changes`

1. Reuse same streaming cursor utility.
2. Keep action-grouped OSC output by buffering change sets.
3. Preserve current ordering within each action (nodes, ways, relations).
4. Preserve current equality semantics.

## Phase 3: cleanup

1. Remove or shrink `owned_elements.rs` if no longer needed.
2. Optionally extract shared streaming merge logic for both commands.

## Risks and gotchas

## 1) Order assumptions across block boundaries

Need robust cursor logic for:

- empty blocks
- mixed dense/node representations
- transitions at exact boundary IDs

## 2) Behavioral compatibility

Must preserve:

- version-insensitive equality behavior
- stats counting semantics
- output formatting (especially `diff --verbose`)

## 3) Backpressure/threading interactions

Two concurrent pipelines (old/new readers) mean:

- two decode pipelines active simultaneously
- potential CPU oversubscription if both use large decode pools

Initial implementation should keep defaults and measure; optional tuning later.

## 4) Error handling clarity

New sorted-input errors should be explicit and actionable:

- mention file path
- mention requirement (`Sort.Type_then_ID`)
- suggest `pbfhogg sort`

## Measurement and validation plan

## Correctness tests

Add/extend tests for:

- identical files
- create/modify/delete cases per element type
- `diff --verbose` details
- action grouping in OSC output
- unsorted-input rejection

Compare outputs against current implementation on small fixtures before removing old path.

## Performance checks

Measure (sequentially, one run at a time):

- peak RSS for `diff` and `derive_changes`
- wall time on Denmark and at least one larger dataset
- behavior with and without type filters (`diff`)

Success criteria:

- no full-file materialization
- bounded memory independent of total file size
- no significant regression for normal (country-scale) inputs

## Additional optimization opportunities (uncertain/theoretical)

## A) Zero-copy compare path from borrowed elements

Instead of materializing owned elements per cursor item, compare borrowed element views directly and only allocate when emitting output.

- Potential win: lower allocation pressure.
- Risk: much more lifetime-heavy code and more complex abstractions.

## B) Shared thread-pool control across dual readers

Explicitly tune decode threads per reader when both files stream simultaneously.

- Potential win: avoid oversubscription on high-core systems.
- Uncertain: pipeline is often consumer-bound; gains may be small.

## C) Optional non-grouped OSC output mode

Allow emitting changes in encounter order for strict streaming, avoiding action buffers.

- Potential win: near-constant memory in `derive_changes`.
- Risk: output style change vs existing grouped format (even if valid XML/OSC).

## D) External spill for large change sets

If action buffers exceed threshold, spill to disk.

- Potential win: bounded memory even for huge diffs.
- Cost: complexity and temporary-file management.

## Recommended next move

~~Implement **`diff` streaming first** (lower output-format complexity), then port the same streaming core to `derive_changes`.~~

**Done.** Both `diff` and `derive_changes` now use streaming merge-join.
`owned_elements.rs` cleaned up (dead code removed).
