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

## Current state (2026-04-27)

Baselines landed in the 2026-04-26 overnight run (commit `16e3694`,
plantasjen, `--bench 1`). Full cross-dataset matrix in
[`reference/performance.md`](../reference/performance.md) under the
new "Merge-changes" section. Headline numbers:

| Dataset | OSC count | Wall | Per-OSC rate | UUID |
|---|---:|---:|---:|---|
| Germany | 1 (`--osc-seq 4705`) | **2.5 s** | 2.5 s | `1ba15f41` |
| Germany | 7 (`--osc-range 4706..4712`) | **18.0 s** | 2.6 s/OSC | `91cb8465` |
| Europe | 7 (`--osc-range 4716..4722`) | **153.2 s** | 21.9 s/OSC | `993ae62a` |
| Planet | 1 (`--osc-seq 4913`) | **43.1 s** | 43.1 s | `76f78e8b` |
| Planet | 7 (`--osc-range 4914..4920`) | **267.2 s (4m27s)** | 38.2 s/OSC | `bef0f1fa` |

`--simplify` adds near-zero overhead at every scale (planet 7-OSC
262.2 s vs 267.2 s default; UUID `c0d140b6`); the `BTreeMap` dedupe
is cheap relative to the per-OSC parse cost.

**Per-OSC scaling is essentially linear**: planet 7-OSC = 6.2× the
1-OSC wall; germany 7-OSC = 7.2× the 1-OSC wall. There's no batching
benefit in the current serial-across-inputs shape - each input pays
its full parse cost. This confirms the parallel-parse opportunity
is real, and it's substantially larger than originally sized (see
"Parallel OSC parse" below).

Production serial-across-inputs shapes today (both in
[`src/commands/merge_changes/mod.rs`](../src/commands/merge_changes/mod.rs)):

- **Streaming path** - `write_streaming` loops over inputs calling
  `parse_osc_streaming(path, ...)` one file at a time, decoding
  gzip + quick-xml and writing events straight through an `OscWriter`.
  No in-memory overlay.
- **Simplify path** - `parse_one_into_stream` builds a per-input
  `ChangeStream`, which feeds a global `BTreeMap` dedupe in
  `write_simplified`. Still serial across inputs in the parse stage.

Both scale linearly with OSC count - the matrix above measures
exactly that.

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

**Sizing against the measured 2026-04-26 baseline:**

- Serial 7-OSC planet parse ≈ 7 × 38.2 s = 267 s wall (matches the
  measured `bef0f1fa` 267.2 s within rounding - confirms parse-cost
  is essentially the entire wall, write tail is negligible).
- Concurrent 7-OSC parse ceiling ≈ max(per-OSC parse) + merge ≈
  the 1-OSC wall (43.1 s) plus a small merge pass.
- **Estimated win: ~210-225 s at planet 7-OSC scale**. The reviewer
  2 Q7 estimate of 20-30 s was sized before the baseline existed
  and turns out to have been an order of magnitude low - it
  assumed parse was 30-40 s of total wall, but the actual
  serial-parse share is ~265 s of 267 s.

The 1-OSC planet bench at 43.1 s is the floor for the
"max(per-OSC parse)" term - the parallel path cannot beat 43 s on
this workload regardless of OSC count. Below that floor would
require parallelizing the per-OSC parse itself (gzip decompress
concurrent with XML parse), which is a separate item.

No win at 1-OSC scale. The feature only fires when the input slice
has length > 1, and scales with input count. The 1-OSC planet bench
above pins this empirically: the wall is per-OSC parse, no
concurrency to extract.

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

1. ~~**Measure current state.**~~ Landed 2026-04-26 overnight at
   commit `16e3694`. Cross-dataset matrix in the "Current state"
   section above and in
   [`reference/performance.md`](../reference/performance.md). The
   `--hotpath` planet run was preflight-refused (estimated 184 GB
   exceeds the 28 GB host RAM); re-bench with `--no-mem-check` to
   capture per-OSC parse spans.
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
