# Business Logic Consolidation Deep Dive (2026-03-03)

## Scope

This review looked for consolidation opportunities larger than the current TODO items, with emphasis on repeated command-level business logic and cross-command dataflow patterns.

## What Is Repeated The Most

1. Element rewrite loops and `BlockBuilder` emit logic are duplicated across many commands.
- `src/commands/cat.rs:175`
- `src/commands/getid.rs:277`
- `src/commands/tags_filter.rs:272`
- `src/commands/extract.rs:761`
- `src/commands/add_locations_to_ways.rs:526`
- `src/commands/merge.rs:439`
- `src/commands/sort.rs:623`

2. Batch/pipeline scaffolding is also duplicated heavily.
- `into_blocks_pipelined()` call sites are spread across `extract/getid/tags_filter/cat/add_locations_to_ways`.
- `map_init(...)` + `drain_batch_results(...)` appears repeatedly in `cat/getid/tags_filter/extract/add_locations_to_ways/merge`.

3. Raw frame passthrough/decode orchestration exists in multiple independent implementations.
- Shared version in `src/commands/mod.rs:92`
- Separate version in `src/commands/cat.rs:73`
- A second two-phase header/data reader in `src/commands/add_locations_to_ways.rs:195`
- Independent passthrough/write orchestration in `src/commands/merge.rs:688` and `src/commands/sort.rs:377`

4. Owned element models and merge-join loops are duplicated.
- Owned models in `src/commands/owned_elements.rs:14`
- Second owned model family in `src/commands/sort.rs:72`
- Parallel merge-join implementations in:
  - `src/commands/diff.rs:121`
  - `src/commands/derive_changes.rs:126`

## Large Consolidation Opportunities

## 1) Introduce a Shared Command Rewrite Engine

### Problem
Most transform commands implement the same high-level runtime:
- stream blocks
- optionally do one or more ID-collection passes
- batch blocks
- run parallel per-block transform
- drain outputs in-order
- merge stats

This shape is independently maintained in `extract`, `tags_filter`, `getid`, `cat`, and part of `add_locations_to_ways`.

### Refactor
Create a shared engine in `src/commands/rewrite_engine.rs`:
- `run_single_pass_rewrite(...)`
- `run_two_pass_rewrite(...)`
- `run_three_pass_rewrite(...)` (or composable pass runner)
- typed hooks for `collect_pass(block, state)` and `rewrite_block(block, ctx, bb, out) -> Stats`

### Why It Is High Value
- Removes large duplicated control flow.
- Centralizes ordering guarantees and batch semantics.
- Makes new commands cheaper to implement and safer.

### Risk
Medium: many call sites, but behavior-preserving if introduced incrementally.

### Planet-Scale Cost Estimate (75GB+)
- Runtime (steady-state after migration):
  - Expected overhead: 0% to +1% if the engine is zero-cost generic/monomorphized.
  - Regression risk: +2% to +5% if trait-object dispatch or extra indirection is used in per-block hot paths.
- Runtime (during transition period):
  - Likely neutral; dual paths increase maintenance cost more than CPU cost.
- Memory usage:
  - Additional resident memory: ~0 to +8 MB (engine structs, pass contexts, temporary vectors already exist today).
- Memory churn:
  - Neutral to slight improvement (-1% to -3% alloc churn) if batching/reuse is centralized.
  - Slight regression possible (+1% to +3%) if closures allocate intermediate state per block.

## 2) Introduce a Shared ElementEmitter API

### Problem
Node/way/relation emission is repeated with minor variants (capacity checks, metadata extraction, tag/ref/member buffers).

There are dozens of repeated `if !bb.can_add_* { flush_local(...) }` + `bb.add_*` paths across files listed above.

### Refactor
Create `ElementEmitter` wrapper around `BlockBuilder`:
- reusable `tags_buf`, `refs_buf`, `members_buf`
- helpers: `emit_dense_node`, `emit_node`, `emit_way`, `emit_relation`
- pluggable metadata mode (`decoded`, `raw`, `none`)

### Why It Is High Value
- Large reduction in copy-pasted, error-prone element-writing code.
- Easier to enforce consistent metadata/tags/member handling.

### Risk
Low/Medium: internal API addition, can be adopted command-by-command.

### Planet-Scale Cost Estimate (75GB+)
- Runtime (steady-state):
  - Expected improvement: -1% to -4% wall time from fewer repeated small allocations and better buffer reuse.
  - Worst case: 0% to +1% if abstraction prevents inlining.
- Memory usage:
  - Additional resident memory: ~0 to +2 MB (shared emitter state per worker thread).
- Memory churn:
  - Expected reduction: -5% to -15% alloc/free events in rewrite-heavy commands (`cat/getid/tags_filter/extract/add-locations`).
  - Main source: shared `tags/refs/members` buffers reused in one place.

## 3) Unify Raw Frame Processing Into One Blob Pipeline

### Problem
Merge, sort, cat, and add-locations each implement similar but separate raw-frame logic (read/classify/passthrough/decode/rewrite/copy-range/coalescing).

### Refactor
Add `src/commands/blob_pipeline.rs`:
- common `FrameSource` (raw mode and two-phase header/data mode)
- common passthrough sink with coalescing and optional copy-range
- common decode-job batching with per-job callback
- preserve command-specific classification policy via trait/callback

### Why It Is High Value
- Eliminates four independent implementations of fragile I/O framing logic.
- Consolidates Linux-specific copy-range behavior in one place.

### Risk
Medium/High: touches performance-critical paths (`merge`, `sort`, `add-locations`).
Use staged adoption: cat first, then sort, then add-locations, then merge.

### Planet-Scale Cost Estimate (75GB+)
- Runtime (steady-state):
  - Best case: -2% to -8% by unifying passthrough/coalescing/copy-range logic and reducing duplicated framing work.
  - Regression risk: +3% to +12% if unified pipeline adds branchy generic dispatch in blob hot paths.
- Runtime (command sensitivity):
  - `cat`: low risk (mostly passthrough), expected -1% to -3% or neutral.
  - `sort`: medium risk; overlap-run decode path is sensitive to extra copies.
  - `add-locations` and `merge`: high sensitivity; regressions here directly affect largest production runs.
- Memory usage:
  - Additional resident memory target: +0 to +16 MB.
  - If done poorly (extra framed-buffer copies): can spike +64 MB to +256 MB per batch window.
- Memory churn:
  - Best case: -10% to -25% churn in raw-frame/passthrough path.
  - Worst case: +10% to +30% churn if frame ownership is copied instead of moved.

## 4) Build a Generic Merge-Join Core for `diff`/`derive_changes` (and future streaming)

### Problem
`diff` and `derive_changes` both:
- load owned vectors
- sort by ID
- run per-type merge-join loops with near-identical control flow

Also, `sort` defines another owned element family.

### Refactor
Add generic merge-join utility:
- `merge_join_by_id(old, new, on_delete, on_create, on_match)`
- share across `diff` and `derive_changes`
- align types with a single owned domain model (or conversion boundary)

Then connect to the streaming merge-join design already documented in [derive-changes-diff-streaming-merge-join.md](/home/folk/Programs/pbfhogg/notes/derive-changes-diff-streaming-merge-join.md).

### Why It Is High Value
- Immediate de-duplication in two user-facing commands.
- Sets up planet-safe streaming path with one merge-join core.

### Risk
Medium: user-visible output stability must be verified with snapshot tests.

### Planet-Scale Cost Estimate (75GB+)
- Runtime (current in-memory mode):
  - Merge-join core extraction itself: ~0% to +1% overhead if generic and inlined.
  - Potential improvement: -1% to -3% by removing duplicated per-type loop code and centralizing branch behavior.
- Runtime (future streaming mode enablement):
  - Enables asymptotic win by avoiding full materialization; this is the major payoff.
- Memory usage:
  - In-memory mode remains dominant (many GB to tens of GB). Core refactor does not materially change this.
  - Streaming follow-up can reduce peak memory from "file-size scale" to "window/buffer scale" (orders of magnitude).
- Memory churn:
  - Small reduction (-2% to -6%) if shared merge-join code reuses comparator/output scratch state.

## 5) Extract a Shared Dependency-Closure Planner (IDs/Refs Graph)

### Problem
`getid --add-referenced`, `tags-filter` (2-pass), and `extract` (complete/smart) all implement variations of:
- seed match sets
- expand dependencies (way->node, relation->way/node)
- second/third pass write from collected sets

### Refactor
Add planner abstraction, e.g. `DependencyClosurePlan`:
- configurable seeds (node/way/relation predicates)
- configurable expansion rules (include way refs, include relation way members, recursive relation handling policy)
- returns read-only ID sets for rewrite passes

### Why It Is High Value
- Removes repeated closure logic that is currently fragmented.
- Easier to add new commands that depend on the same graph expansion semantics.

### Risk
Medium: careful validation needed for `extract smart` semantics.

### Planet-Scale Cost Estimate (75GB+)
- Runtime (steady-state):
  - Neutral to moderate win: -1% to -6% if closure expansion passes are fused/reused.
  - Regression risk: +2% to +10% if planner generalization introduces extra pass bookkeeping or dynamic rule dispatch.
- Memory usage:
  - Additional resident memory: +8 MB to +64 MB for generalized closure state/maps, depending on representation.
  - Must keep dense bitsets as primary backing for node-scale sets to avoid explosive growth.
- Memory churn:
  - Can improve (-5% to -20%) if pass-local temporary sets become reusable pooled structures.
  - Can regress (+5% to +25%) if planner constructs transient boxed rule graphs per pass.

## 6) Normalize Command Option Models for I/O Modes

### Problem
`SortOptions` and `MergeOptions` duplicate the same I/O mode fields:
- `compression`, `direct_io`, `io_uring`, `sqpoll`, `force`

### Refactor
Introduce common internal options struct (for command internals), e.g. `IoModeOptions`, and embed/compose it in command-specific option types.

### Why It Is Useful
- Smaller than the items above, but reduces drift in behavior and defaults.

### Risk
Low.

### Planet-Scale Cost Estimate (75GB+)
- Runtime:
  - Effectively neutral (0% expected).
- Memory usage:
  - Neutral (configuration structs only).
- Memory churn:
  - Neutral.

## Suggested Refactor Sequence

1. `ElementEmitter` (low risk, high churn reduction immediately).
2. `rewrite_engine` for one command family (`getid` + `tags_filter`) first.
3. Extend engine to `extract`.
4. Introduce generic merge-join core for `diff` + `derive_changes`.
5. Unify blob pipeline (`cat` -> `sort` -> `add_locations_to_ways` -> `merge`).
6. Add dependency-closure planner last (after rewrite engine stabilizes).

## Expected Impact

- Meaningful reduction in command code size and duplicate maintenance burden.
- Better consistency of ordering/flush behavior across commands.
- Lower risk of regressions when changing shared concerns (metadata handling, flush boundaries, passthrough policy).
- Cleaner path to large pending work (streaming diff/derive, extract/simple redesign) by reusing shared engines instead of re-implementing flow control.
