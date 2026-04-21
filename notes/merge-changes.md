# `merge-changes` - optimization plan

Target: `pbfhogg merge-changes` - squashes N OSC (gzip + XML) inputs into
one OSC output. Used as an upstream stage before `apply-changes` when a
production pipeline needs to apply accumulated diffs (e.g. a week of
dailies) rather than a single daily.

Content here was factored out of
[`apply-changes-opportunities.md`](apply-changes-opportunities.md) on
2026-04-21, where these items had been filed under "weekly apply-changes"
because that was the scenario that forced the question. The underlying
optimizations apply to `merge-changes` directly and benefit any
consumer that squashes N > 1 OSCs, scale-independently.

## Current state (2026-04-21)

Unmeasured at planet-range OSC sets. The CLI exercises the path:

```
brokkr merge-changes --dataset planet --osc-range 4914..4920 --bench 1
```

No `.brokkr/results.db` rows at this config yet. Measurement is the
first prerequisite before shipping any optimization here.

Two shapes of serial work exist in the codebase today:

- [`merge_changes::write_streaming`](../src/commands/merge_changes/mod.rs)
  loops over inputs and calls `parse_osc_streaming(path, ...)` one file
  at a time. gzip + quick-xml decode is single-threaded per input, and
  the outer loop serialises across inputs. At 7× OSCs this compounds
  linearly.
- [`osc::load_all_diffs`](../src/osc.rs) is the library-level
  equivalent for in-memory overlay construction: builds a single
  `CompactDiffOverlay` by parsing each OSC sequentially with
  newer-wins semantics across duplicate element IDs. Any library
  consumer that takes a slice of OSC paths funnels through this.

Both scale linearly with OSC count.

## Opportunities

### Parallel OSC parse

(Relevant to both `write_streaming` and `load_all_diffs`; they share
the serial-across-inputs shape.)

Parse each OSC concurrently into its own overlay; merge overlays with
newer-wins semantics using
[`IdSetDense::set_atomic_if_new`](../src/commands/id_set_dense.rs#L163)
as the per-element-type dedupe primitive. Walk overlays newest-first;
keep the element iff `set_atomic_if_new` returns `true` (atomic
fetch-or under shared `&self`). `set_if_new` is the non-atomic variant
for single-threaded paths. Pre-allocate per element type via
`IdSetDense::pre_allocate(max_id)`.

Each OSC is independent work. The merge pass is a few seconds over
the combined overlays; parallel parse scales with OSC count up to
available cores.

Estimated **~20-30 s wall saved at 7-OSC planet scale** (reviewer 2's
Q7 analysis from the apply-changes round): serial
`load_all_diffs` ≈ 30-40 s becomes `max(per-OSC parse, merge pass)` ≈
10-15 s. Bounds are speculative - needs confirmation by running an
actual planet-range bench once current wall is measured.

No win at 1-OSC scale. The feature only fires when the slice has
length > 1, and scales with input count.

### Diff squashing as a formal upstream stage (already shipped as `merge-changes`)

Reviewer 1 (apply-changes Q7 round) flagged "diff squashing as a
formal upstream stage" as the right long-term shape if accumulated-diff
batching is the standard cadence: a separate command that runs once
per cadence and emits a single pre-merged OSC that `apply-changes`
then consumes as if it were a daily.

That command already exists - it's `merge-changes` itself. The
"opportunity" here is really:

1. **Measurement** - confirm current `merge-changes` wall at
   planet-range OSC sets. Without a number, we can't say whether the
   parallel-parse optimisation above is worth implementing.
2. **Documentation** - recommend the `merge-changes → apply-changes`
   pipeline pattern in the README usage section for batched-diff
   consumers, so users don't end up writing one-off scripts that
   re-do `load_all_diffs`'s work.

Both are cheap follow-ups once we have a wall baseline.

Primitive shared with parallel OSC parse above:
`IdSetDense::set_atomic_if_new` for newer-wins duplicate detection.

## Prerequisites before shipping anything

1. **Measure current state.** `brokkr merge-changes --dataset planet
   --osc-range 4914..4920 --bench 1` is already wired up; just run it
   and store the wall + RSS in `.brokkr/results.db`. Without a
   baseline, wall-time claims are speculative.
2. **Per-input parse span instrumentation.** Add
   `MERGE_CHANGES_PARSE_START/END` markers around each
   `parse_osc_streaming(path, ...)` call so the parallel-parse design
   can be compared against the per-OSC share of serial wall. Same
   pattern as the `MERGE_*` markers in
   [`apply_changes/rewrite.rs`](../src/commands/apply_changes/rewrite.rs).
3. **Confirm `load_all_diffs` call sites.** If only `merge-changes`
   and `apply-changes` consume it today, the optimization surface is
   small and both land together. If other consumers exist, scope the
   change to the shared function.

## Cross-references

- [`apply-changes-opportunities.md`](apply-changes-opportunities.md) -
  the original home of this content; retains `merge_osc_parse_ms`
  counter definition and the historical Q7 reviewer analysis that
  motivated the parallel-parse item.
- [`src/commands/id_set_dense.rs`](../src/commands/id_set_dense.rs) -
  `IdSetDense::set_atomic_if_new` and `pre_allocate(max_id)` APIs.
- [`src/osc.rs`](../src/osc.rs) - `load_all_diffs`, the library-level
  serial merger.
- [`src/commands/merge_changes/mod.rs`](../src/commands/merge_changes/mod.rs)
  - `write_streaming`, the CLI-level serial merger (different shape:
  writes XML output directly rather than building an in-memory
  overlay).
