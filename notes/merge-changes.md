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

## Current state (2026-04-23)

No `.brokkr/results.db` rows at planet-range OSC sets yet.
`overnight.sh:246` runs
`brokkr merge-changes --dataset planet --osc-range 4914..4920 --bench 1`
plus `--hotpath` tonight, so the baseline lands tomorrow morning.

Production serial-across-inputs shapes today (both in
[`src/commands/merge_changes/mod.rs`](../src/commands/merge_changes/mod.rs)):

- **Streaming path** - `write_streaming` loops over inputs calling
  `parse_osc_streaming(path, ...)` one file at a time, decoding
  gzip + quick-xml and writing events straight through an `OscWriter`.
  No in-memory overlay.
- **Simplify path** - `parse_one_into_stream` builds a per-input
  `ChangeStream`, which feeds a global `BTreeMap` dedupe in
  `write_simplified`. Still serial across inputs in the parse stage.

Both scale linearly with OSC count.

The library-level `osc::load_all_diffs`
([`src/osc/parse.rs:315`](../src/osc/parse.rs#L315)) also has this
shape, but is **test-only** today - no production call site consumes it
(confirmed 2026-04-23). `apply-changes` takes a single OSC via
`parse_osc_file`, so the "per-input" axis doesn't apply there.
Parallelisation work should target the two production shapes above,
not `load_all_diffs`.

## Opportunities

### Parallel OSC parse

Parse each OSC concurrently; merge results with newer-wins semantics
in sequence order. Per-element dedupe primitive:
[`IdSet::set_atomic_if_new`](../src/idset.rs) (atomic fetch-or under
shared `&self`). `IdSet::set_if_new` is the non-atomic variant for
single-threaded paths. Pre-allocate per element type via
`IdSet::pre_allocate(max_id)`.

Two plausible shapes depending on which production path is being
optimised:

- **Simplify path**: each worker parses one OSC into its own
  `ChangeStream`; main thread drains the per-worker streams into the
  `BTreeMap` in sequence order so "later wins" survives. The
  `set_atomic_if_new` primitive can replace the `BTreeMap` entirely
  if measurements show it dominating the wall.
- **Streaming path**: the output is event-ordered XML, not an
  overlay, so "parse in parallel, write sequentially" only helps if
  gzip decompress and XML parse are both concurrent with the writer.
  The simpler win may be parallel gzip decompress + sequential XML
  parse if the hotpath breakdown shows decompress dominating.

Each OSC is independent work. The merge / write pass is a few seconds
over the combined content; parallel parse scales with OSC count up to
available cores.

Estimated **~20-30 s wall saved at 7-OSC planet scale** (reviewer 2's
Q7 analysis from the apply-changes round): serial parse ≈ 30-40 s
becomes `max(per-OSC parse, merge pass)` ≈ 10-15 s. Bounds are
speculative - needs confirmation from the tomorrow-morning baseline
and the per-OSC `MERGECHANGES_PARSE_{START,END}` spans.

No win at 1-OSC scale. The feature only fires when the input slice has
length > 1, and scales with input count. Overnight.sh also runs
`--osc-seq 4913` (1-OSC planet) to pin the "no win at 1-OSC"
speculation.

### Parallel gzip output

`write_streaming` and `write_simplified` both write gzip-compressed
XML on the main thread. If the hotpath breakdown shows the
`MERGECHANGES_WRITE_FINISH_{START,END}` span is a non-trivial fraction
of wall, substitute
[`ParallelGzipWriter`](../src/write/parallel_gzip.rs) (already proven
in `diff --format osc` assembly - ~10% wall win there). No API change
to callers; it implements `Write`.

Gated on tomorrow's sidecar data - if the writer tail is under ~15%
of wall, deprioritise.

### Simplify dedupe: BTreeMap alternatives

`write_simplified` uses a global `BTreeMap` for "last change per
object" dedupe. At N inputs naturally ordered by sequence number,
a multi-input merge walk with first-occurrence-wins could replace the
global sort. Or an `FxHashMap` keyed by `(kind, id)` holding the
latest change, followed by a single sort at the end.

Expected small win (5-10% of `--simplify` wall). Low priority until
overnight data says simplify is hot enough to matter.

### `merge-changes` as the formal upstream-diff-squash stage

Reviewer 1 (apply-changes Q7 round) flagged "diff squashing as a
formal upstream stage" as the right long-term shape if
accumulated-diff batching is the standard cadence. That command is
`merge-changes` itself. The practical follow-up is documentation:
recommend the `merge-changes → apply-changes` pipeline pattern in
README so users don't end up writing one-off scripts that re-do
this work.

## Prerequisites before shipping anything

1. **Measure current state.** Scheduled: `overnight.sh:246`
   (`brokkr merge-changes --dataset planet --osc-range 4914..4920
   --bench 1` + `--hotpath`). Baseline lands morning of 2026-04-24.
2. ~~**Per-input parse span instrumentation.**~~ Landed 2026-04-22
   (commit `4e3c7ea`). `MERGECHANGES_PARSE_{START,END}` wrap each
   `parse_osc_streaming` and `parse_osc_into` call on both
   production paths. `merge_changes_input_bytes` counter inside each
   span for size distribution. `#[hotpath::measure]` on parse and
   write functions for per-function wall.
3. ~~**Confirm `load_all_diffs` call-site scope.**~~ Resolved
   2026-04-23: `load_all_diffs` is test-only in this repo; no
   production consumers. Parallelisation target is the two
   production shapes in
   [`src/commands/merge_changes/mod.rs`](../src/commands/merge_changes/mod.rs).

## Cross-references

- [`apply-changes-opportunities.md`](apply-changes-opportunities.md) -
  the original home of this content; retains `merge_osc_parse_ms`
  counter definition and the historical Q7 reviewer analysis that
  motivated the parallel-parse item.
- [`src/idset.rs`](../src/idset.rs) - `IdSet::set_atomic_if_new` and
  `pre_allocate(max_id)` APIs. (Renamed from `IdSetDense` in 0.3.0;
  moved from `getid` to top-level `pbfhogg::idset`.)
- [`src/osc/parse.rs`](../src/osc/parse.rs) - `parse_osc_file`,
  `parse_osc_file_into`, `load_all_diffs` (test-only).
- [`src/commands/merge_changes/mod.rs`](../src/commands/merge_changes/mod.rs) -
  `write_streaming` (streaming path) and `parse_one_into_stream` /
  `write_simplified` (simplify path). The two production
  serial-across-inputs shapes.
- [`src/write/parallel_gzip.rs`](../src/write/parallel_gzip.rs) -
  `ParallelGzipWriter`, candidate for the writer-tail opportunity.
