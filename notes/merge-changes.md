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

Parse each OSC concurrently; merge results in sequence order. Per-input
work is independent; the join is a deterministic main-thread drain.

**Pre-flight measurement (2026-04-28, commit `fb1719c`, UUID
`c612c5e6`):** Added `merge_changes_decompress_ns` (via a
`TimedRead<R>` wrapper around the file/gzip reader) and
`merge_changes_changes_per_osc` (per-input change-count delta) to
both production parse shapes. Planet 7-OSC re-bench:

| OSC | Input MB | Wall (s) | Decompress (s) | Decompress % | Changes |
|----:|---------:|---------:|---------------:|-------------:|--------:|
|   1 |     97.3 |    39.59 |          0.576 |         1.5% | 3.74 M |
|   2 |     97.1 |    38.67 |          0.578 |         1.5% | 3.86 M |
|   3 |    130.2 |    52.54 |          0.785 |         1.5% | 5.80 M |
|   4 |     87.1 |    34.71 |          0.513 |         1.5% | 3.41 M |
|   5 |     72.7 |    29.70 |          0.429 |         1.4% | 2.62 M |
|   6 |     80.2 |    31.50 |          0.468 |         1.5% | 3.06 M |
|   7 |    106.2 |    45.87 |          0.629 |         1.4% | 3.78 M |
| **sum** | **670.9** | **272.57** | **3.98** | **1.5%** | **26.26 M** |

**Two findings reshape the implementation choice.**

1. **Gzip decompress is 1.5 % of wall.** The "parallel gzip
   decompress + sequential XML parse" alternative is dead - there is
   nothing to win there. The 98.5 % remainder is XML parse + output
   XML emit + output gzip-compress, all interleaved on one thread in
   the streaming shape.
2. **The win must come from parallel XML parsing**, which forces a
   buffer-and-drain shape: each worker parses its OSC into a local
   `ChangeStream`, the main thread iterates streams in input order
   and emits XML to the writer.

**Decision: buffer-and-drain.** Both production paths take the same
shape:

- **Streaming path**: each worker calls `parse_osc_into` (already
  exists) to build a local `ChangeStream`. Main thread drains
  streams[0..N] in input order, emitting XML to the existing
  `OscWriter` via the existing `emit_change` /
  `write_change_to` helpers. `open_action` grouping flows across
  the drain so the action-tag elision optimization survives.
- **Simplify path**: each worker calls `parse_osc_into` into a
  local `ChangeStream`. Main thread inserts streams[0..N] into the
  global `BTreeMap` in input order, so "later inputs win" semantics
  survive (the existing serial loop already relies on insertion
  order). Stage afterwards is unchanged.

**Per-worker `ChangeStream` peak memory.** Largest single OSC at
planet 7-OSC: 5.80 M changes. Average residual per `Change` (Action
+ enum-wrapped `OwnedNode` / `OwnedWay` / `OwnedRelation` with
inline tags / refs / members) is hard to size exactly without an
RSS sample, but a crude estimate from typical OSC content is
~120-200 B / change residual after parser scratch is freed, putting
the per-worker peak at ~700 MB - 1.2 GB for OSC 3, lower for
others. **All-7-streams-buffered ceiling: ~4 GB.** Comfortable on
the 23 GB reference host (plantasjen). Pipelining the drain
(begin emitting streams[0] while streams[1..6] are still parsing)
trims peak to ~2-3 in-flight streams (~1.5-2 GB) but is an
optimization, not a correctness requirement.

**Revised win estimate.** The original sizing
(~210-225 s, "ceiling = max(per-OSC parse) + merge") missed that
the streaming shape currently emits XML + gzip-compresses output
*during* parse. Once parse is decoupled, the drain pass becomes a
new serial cost: re-walking 26.26 M changes, emitting XML, and
gzip-compressing 968 MB of output. Output gzip-compress alone
(zlib level 6, 968 MB output) is in the 20-60 s range based on
zlib throughput on this host.

- Max per-OSC parse from `c612c5e6` table: 52.5 s (OSC 3).
- Drain pass estimate: 30-60 s (XML emit + zlib output).
- **New ceiling: ~80-115 s at planet 7-OSC scale**, vs the 272.6 s
  baseline. **Estimated win: ~160-190 s, ~2.4-3.4x speedup.**

If the drain-pass gzip-compress dominates the new wall, swapping
the output through `ParallelGzipWriter`
([`src/write/parallel_gzip.rs`](../src/write/parallel_gzip.rs))
recovers the bulk of it (proven ~10 % win in `diff --format osc`
assembly, larger here because output is pure-zlib without any
upstream parallelism stealing cores).

The 1-OSC planet bench at 43.1 s is **not** the floor for the
parallel path under the new sizing - decoupling parse from output
introduces a drain cost that the 1-OSC serial wall does not pay.
Below ~80 s on planet 7-OSC requires either a `ParallelGzipWriter`
output or pipelining drain with parse.

No win at 1-OSC scale. The feature only fires when the input slice
has length > 1.

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
   [`reference/performance.md`](../reference/performance.md).
   `--hotpath` + `--alloc` captured 2026-04-27 overnight at `4fc8e35`
   (UUIDs `ee108ec9` / `13615a4a`) once brokkr's preflight memory
   check was removed: 100 % of wall in 7 calls to `parse_osc_streaming`,
   per-OSC avg 37.8 s / **P95 50.9 s**, 62.4 GB cumulative alloc all
   in `parse_osc_streaming`.
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
4. ~~**Gzip-vs-XML and per-OSC change-count instrumentation.**~~
   Landed 2026-04-28 (commit `fb1719c`). `TimedRead<R>` wrapper
   accumulates `read()` wall time so `merge_changes_decompress_ns`
   attributes gzip work separately from the surrounding
   quick-xml machinery. `merge_changes_changes_per_osc` emitted on
   the per-input `count` / `stream.changes.len()` delta sizes the
   per-worker `ChangeStream` peak. Re-bench `c612c5e6` proved
   gzip = 1.5 % of wall, killing the parallel-decompress
   alternative and locking in the buffer-and-drain shape; see the
   measurement table in "Parallel OSC parse" above.

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
