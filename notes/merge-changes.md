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

## Current state (2026-04-28)

**Both production paths parallelized.** Planet 7-OSC headline:

| Path | Pre-parallel | Final | Speedup | Final UUID / commit |
|---|---:|---:|---:|---|
| Streaming, planet 7-OSC | 267.2 s | **54.7 s** | **5.0×** | `b6e964cc` / `99057fa` |
| Simplify, planet 7-OSC | 262.2 s | **73.7 s** | **3.6×** | `3e3ef119` / `abd1d9e` |
| 1-OSC fast path | 43.1 s | 44.2 s | (unchanged) | `941a5784` |

Streaming-path stage history:

| Stage | UUID | Wall | vs baseline |
|---|---|---:|---:|
| Serial baseline (`16e3694`) | `bef0f1fa` | 267.2 s | 1.00× |
| Instrumented re-bench (`fb1719c`) | `c612c5e6` | 272.6 s | 1.00× |
| Parallel parse, serial drain (`43dd620`) | `07ee92ee` | 235.8 s | 1.16× |
| **Parallel-drain (`99057fa`)** | **`b6e964cc`** | **54.7 s** | **5.0×** |

Simplify-path stage history:

| Stage | UUID | Wall | vs baseline |
|---|---|---:|---:|
| Pre-parallel baseline (`16e3694`) | `c0d140b6` | 262.2 s | 1.00× |
| Parallel parse only (`488d1f0`) | `37fbe5b5` | 220.9 s | 1.19× |
| **Parallel parse + parallel write_simplified (`abd1d9e`)** | **`3e3ef119`** | **73.7 s** | **3.6×** |

Cross-dataset matrix in
[`reference/performance.md`](../reference/performance.md) under the
"Merge-changes" section.

Production code paths in
[`src/commands/merge_changes/mod.rs`](../src/commands/merge_changes/mod.rs):

- **Streaming path** (`write_streaming`): N <= 1 keeps the original
  serial pipeline (one-pass parse + emit + gzip). N > 1 does
  parallel-drain: each rayon worker runs the full per-input pipeline
  (parse + XML re-emit + gzip-compress) into its own
  `OscWriter<Vec<u8>>` and returns self-contained gzip bytes. Main
  thread writes a pre-built prelude gzip member, the worker chunks in
  input order, and a postlude gzip member. Multi-member gzip is
  valid; OSC consumers (osmium, osmosis, `MultiGzDecoder`, gzip CLI)
  all support it.
- **Simplify path** (`merge_changes` with `--simplify`): N > 1
  parallelizes the parse phase the same way as streaming AND
  parallelizes `write_simplified`'s output. After the BTreeMap
  dedupe, each non-empty action group (creates / modifies / deletes)
  is split into `available_parallelism`-sized chunks via rayon's
  `par_chunks`; each chunk emits a self-contained
  `<action>...</action>` gzip member; main thread concatenates with
  the same prelude/postlude wrapping as the streaming path. Phase
  breakdown at planet 7-OSC: parse 12.3 s + dedupe 6.9 s + parallel
  emit 49.4 s + drain 0.33 s.

Per-OSC scaling pre-parallel was essentially linear (planet 7-OSC =
6.2× the 1-OSC wall; germany 7-OSC = 7.2× the 1-OSC wall) - each
input paid its full parse cost in serial. The parallel-drain shape
collapses this to `max(per-OSC parse + emit + gzip) + small drain`,
gated by the heaviest single OSC.

The library-level `osc::load_all_diffs`
([`src/osc/parse.rs:315`](../src/osc/parse.rs#L315)) is **test-only**
today - no production call site consumes it (confirmed 2026-04-23).
`apply-changes` takes a single OSC via `parse_osc_file`, so the
"per-input" axis doesn't apply there. Parallelisation work targets
the two production shapes above, not `load_all_diffs`.

## Opportunities

### Parallel OSC parse - LANDED 2026-04-28 (`99057fa`)

Final shape: **parallel-drain via per-worker gzip members.** Each
worker (rayon par_iter at N > 1) runs the full per-input pipeline -
parse + XML re-emit + gzip-compress - into its own
`OscWriter<Vec<u8>>` and returns `(Vec<u8>, count)`. Main thread
writes a pre-built XML prelude (`<?xml ?><osmChange version="0.6">`)
gzip member, the worker chunks in input order, and a postlude
(`</osmChange>`) gzip member. Multi-member gzip is valid OSC; the
output decompresses to the concatenation of all members.

#### Pre-flight measurement (2026-04-28, commit `fb1719c`, UUID `c612c5e6`)

`TimedRead<R>` wrapper around the file/gzip reader and
`merge_changes_changes_per_osc` per-input counter. Planet 7-OSC
re-bench:

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

The pre-flight delivered the load-bearing implementation decision:

1. **Gzip decompress is 1.5 % of wall.** Killed the
   parallel-decompress + sequential-XML alternative outright.
2. **The other 98.5 % is XML parse + emit + output gzip-compress**,
   all interleaved on one thread by the streaming shape's
   pull-parser-with-side-effects pattern.

#### Stage history

The work landed in three stages, each with its own UUID. Keeping
the abandoned middle stage in this doc because the failure
diagnosis was the load-bearing input to the final design.

| Stage | Commit | UUID | Wall | vs baseline | Notes |
|---|---|---|---:|---:|---|
| Serial baseline | `16e3694` | `bef0f1fa` | 267.2 s | 1.00× | pre-parallel |
| Instrumented re-bench | `fb1719c` | `c612c5e6` | 272.6 s | 1.00× | gzip / change-count instrumentation |
| Parallel parse, serial drain | `43dd620` | `07ee92ee` | 235.8 s | 1.16× | parse phase 12.6 s (21× phase speedup), drain 223 s (new bottleneck) |
| **Parallel-drain (final)** | **`99057fa`** | **`b6e964cc`** | **54.7 s** | **5.0×** | parallel emit phase 54.1 s, drain (concat) 0.59 s |

The middle stage's 223 s drain came from the main thread doing
per-change `quick_xml::Writer` emit + zlib level-1 gzip-compress for
26.3 M changes, single-threaded ceiling at ~118 K changes/s. Moving
emit + gzip onto the worker threads parallelized that 223 s across
the same 7 workers already doing parse, eliminating the serial
ceiling entirely. The remaining wall is now gated by the heaviest
single OSC (OSC 3 at 5.8 M changes, completing at 54.1 s in
`b6e964cc`).

#### Memory

Per-worker peak: parsed-but-not-yet-emitted intermediate state
(small, since `parse_osc_streaming` fuses parse + emit) plus the
per-worker gz output buffer (~140 MB compressed for the heaviest
OSC). All-7-workers-in-flight peak: ~1 GB compressed bytes,
comfortable on the 23 GB reference host. The earlier
parallel-parse-only shape (`43dd620`) needed ~4 GB of buffered
`ChangeStream` intermediates; the parallel-drain shape eliminates
that intermediate.

#### Output format note

Action-tag elision is local to each worker rather than global, so a
few extra `</modify><modify>` boundaries appear between inputs.
Output remains valid OSC; bytes added are negligible (~50 bytes per
input boundary, ~6 boundaries at planet 7-OSC; final output 962 MB
vs 968 MB serial, well within compression noise). Multi-member gzip
is supported by every production OSC consumer (osmium, osmosis,
gzip CLI, `MultiGzDecoder`); single-member output isn't worth the
re-engineering cost.

#### Follow-ups

- **`--simplify` parallel write_simplified landed 2026-04-28 at
  `abd1d9e`** (UUID `3e3ef119`): **262.2 s -> 73.7 s, 3.6×**, with
  the write_simplified phase itself going from ~197 s to 49.4 s
  (4.0× phase speedup). Implementation mirrors the streaming-path
  parallel-drain: after the BTreeMap dedupe, each non-empty action
  group is split into `available_parallelism`-sized chunks via
  rayon's `par_chunks`, each chunk emits a self-contained
  `<action>...</action>` gzip member, main thread concatenates with
  the shared prelude/postlude wrapping. The intermediate stage at
  `488d1f0` (UUID `37fbe5b5`, 220.9 s, parallel parse only,
  write_simplified still serial) is the load-bearing measurement
  proving the write_simplified phase was the bottleneck (~197 s of
  220.9 s).
- **Pipelined drain.** Workers emit gzip bytes; main thread could
  begin writing worker[0]'s chunk to disk while worker[6] is still
  parsing. Current shape buffers all worker chunks in memory before
  any drain. ~280 MB peak instead of ~1 GB at planet 7-OSC, modest
  win.
- **`merge-changes` as the formal upstream-diff-squash stage**
  (existing item, see below).

No win at 1-OSC scale. The feature only fires when the input slice
has length > 1; the 1-OSC fast path keeps the original serial
streaming pipeline (parse + emit + gzip interleaved on one thread).

### Simplify dedupe: BTreeMap alternatives - RETIRED 2026-04-28

`write_simplified` uses a global `BTreeMap` for "last change per
object" dedupe. The original entry speculated that an `FxHashMap`
keyed by `(kind, id)` (or a multi-input merge walk over already-sorted
inputs) could shave 5-10% of `--simplify` wall.

**Invalidated by `3e3ef119`'s phase breakdown.** With parse and
write_simplified both parallelized, the BTreeMap dedupe is **6.9 s
of 73.7 s** (9.4 % of wall) at planet 7-OSC. A perfect zero-cost
dedupe replacement would shave at most 6.9 s; an `FxHashMap` swap
realistically saves ~3-5 s (40-70 % of dedupe wall). That is below
the noise band of the wall measurement and not the load-bearing
optimization the original entry framed it as. Retired.

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
