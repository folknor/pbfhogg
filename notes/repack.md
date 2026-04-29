# `repack` - command status and v2 scope

Re-encode a PBF with a configurable element-count cap per blob.
Motivated by [`reference/blob-density.md`](../reference/blob-density.md):
the measurement matrix needs same-corpus-different-encoding pairs to
control for blob-count effects independent of byte size.

**Status:** v1 + v2.1 shipped. v1 was the parallel per-worker shrink
path; v2.1 added cross-input-blob coalescing so grow caps actually
fire. v2.2 (LocationsOnWays preservation) and v2.3 (osmium cross-
validation) remain deferred.

## What ships today

Code: [`src/commands/repack/mod.rs`](../src/commands/repack/mod.rs);
CLI: `pbfhogg repack`.

```
pbfhogg repack <input> -o <output> [--elements-per-blob N]
                                    [--compression C]
                                    [--direct-io] [--io-uring]
                                    [--force]
                                    [--generator ...] [--output-header ...]
```

Pipeline shape: parallel three-phase per-kind classify (nodes, then
ways, then relations). Each worker decodes one input blob, filters
to the current kind, and splits its `M` matching elements at the
`M % cap` boundary:

- The leading `M - (M % cap)` elements (a multiple of `cap`) are
  re-encoded through a per-worker `BlockBuilder` and shipped to the
  merge thread as already-framed blob bytes. This is the parallel
  path that recovers v1's shrink throughput.
- The trailing `M % cap` elements are shipped as decoded
  `OwnedNode`/`OwnedWay`/`OwnedRelation` to the merge thread.

The merge thread runs a single long-lived `BlockBuilder` per kind,
configured with the requested cap, and consumes worker outputs in
seq order via a `ReorderBuffer`. For each input blob it writes the
worker's full framed blocks directly, then feeds the trailing
`Owned*` slice into the central builder; mid-stream flushes are
framed in parallel via `rayon::par_iter` over `FRAME_BATCH`-sized
batches and written serially.

This hybrid keeps shrinks fast (when `cap` divides `M` cleanly all
work happens in workers, matching v1) while supporting grows
correctly: when `cap > M_per_input_blob` each worker emits 0 full
blocks and ships everything as trailing, and the central builder
spans input-blob boundaries to produce cap-sized output.

Validated on Denmark (commit 741e482, plantasjen):
- cap=4000 (shrink, exact div): 2.8 s - within noise of v1's 2.7 s.
- cap=8000 (matches input default): 2.9 s - same shape, all
  workers produce one full block each, no merge work.
- cap=16000 (grow, all-tail): 23 s - merge-thread bound.

The grow regression versus shrink is fundamental: when every input
blob's elements end up in the trailing slice, the central builder
on a single thread does all the encoding work. For dev/debug
repacks this is acceptable; if a planet-scale grow ever needs to
be faster, the next intervention is element-offset-aware workers
(shape (2) from the original design sketch), which would require
extending the schedule with per-blob element counts.

### Validated at scale (commit 48685ba, plantasjen, 2026-04-28)

| Dataset | Cap   | Mode  | Wall   | Peak RSS | UUID       |
|---------|-------|-------|--------|----------|------------|
| denmark | 64000 | bench | 23 s   | 983 MB   | `a48e755f` |
| denmark | 64000 | hot   | 18 s   | -        | `e5edccad` |
| denmark | 64000 | alloc | 17 s   | -        | `ebd3083e` |
| europe  | 8000  | bench | 195 s  | 428 MB   | `29d0216c` |
| planet  | 8000  | bench | 380 s  | 1.36 GB  | `0ae01c09` |

Bench-mode peak RSS stays under 1.5 GB even at planet scale - the
per-kind parallel-classify pipeline is bounded per worker, so the
working set does not grow with the input. The planet 8k artefact
(`0ae01c09`) is the input that unblocks
[`reference/blob-density.md`](../reference/blob-density.md) and the
deferred `HeaderWalker` dispatch decision in
[`notes/getparents.md`](getparents.md).

### Known issue: `--hotpath` / `--alloc` OOM at europe and planet scale

`brokkr repack --hotpath` and `brokkr repack --alloc` were OOM-killed
at both europe and planet scale on plantasjen (30 GB RAM, 8 GB swap)
during the 2026-04-28 overnight run. The kernel oom-killer logged
peak anon-rss between 28.9 GB and 29.1 GB on all four killed
processes (plantasjen kernel log, 21:53-22:04). Bench mode at the
same dataset and cap landed at <1.5 GB peak, so the regression is
from the brokkr profiler modes, not the repack pipeline itself.

Denmark `--hotpath` / `--alloc` complete fine (`e5edccad`, `ebd3083e`),
so the overhead scales with element throughput rather than being a
fixed constant.

`repack` should be planet-scale safe in every measurement mode; this
is a memory-safety regression to fix, not a documented limit.

Workarounds until it lands:
- Bench mode is planet-safe at any cap.
- Denmark / Norway / Switzerland still produce useful `--hotpath` and
  `--alloc` profiles.

### Inputs and flags

- `--elements-per-blob N` (default 8000). Zero is rejected up front.
- `--compression C`: passthrough to `PbfWriter`. Useful for A/B
  against zstd vs zlib at a fixed blob size.
- `--direct-io` / `--io-uring`: standard write-path flags.
- `--force`: skip the indexdata requirement (slower path).
- `--generator` / `--output-header`: standard header overrides.

### Indexdata requirement

Input must have blob-level indexdata (use `brokkr download` or pass
`--force`). The classify pipeline needs per-kind blob schedules,
which it derives from indexdata.

### Markers and counters

The phase boundaries emit
`REPACK_NODES_START` / `_END`, `REPACK_WAYS_START` / `_END`,
`REPACK_RELATIONS_START` / `_END`, and end-of-run counters
`repack_blobs_written`, `repack_elements_written`, and
`repack_input_blobs_coalesced` (visible via `brokkr sidecar <uuid>`).

The `repack_input_blobs_coalesced` counter increments every time an
input blob's trailing slice arrives at the merge thread with the
central builder already non-empty, i.e. that input blob's elements
extended a prior input blob's residuals inside a single output
blob. 0 on shrinks with exact division; otherwise grows with the
proportion of input blobs that don't divide cleanly.

### Grow no-op detection

The "never fired" warning still exists but its scope tightened with
v2.1. It now fires only when the cap exceeds every kind's total
element count - i.e. each kind collapses to a single output blob and
no mid-stream flush ever happened anywhere:

```
Warning: --elements-per-blob N never fired; every per-kind element
count fits in a single output blob, so the output is one blob per kind.
```

The pre-v2.1 message ("cap exceeds the largest input blob, so the
output blob layout matches the input") is no longer accurate: with
cross-input-blob coalescing, the input blob layout no longer
constrains the output.

### Limitation: LocationsOnWays not preserved

If the input header declares `LocationsOnWays`, the run prints a
stderr warning that inline way-node coordinates will not be
propagated. The output PBF does not carry the LOW feature flag and
does not embed coordinates in way refs. This is the only metadata
the round-trip drops.

### What v1 preserves

- Element IDs, tags, and OsmMetadata (version, timestamp, changeset,
  uid, user, visible) for every kind.
- Way refs (delta-encoded node IDs).
- Relation members (id + type + role).
- DenseNode encoding (DenseNode in -> DenseNode out).
- `Sort.Type_then_ID` header flag when the input has it.
- All `OsmSchema-V0.6` / `DenseNodes` / `HistoricalInformation`
  features that pass through the standard writer header path.

### Tests

[`tests/cli_repack.rs`](../tests/cli_repack.rs):

- `repack_round_trip_preserves_elements_on_shrink` - element/ID/tag
  multiset equality after a shrink (cap=10 vs input 20/blob).
- `repack_respects_element_cap` - every output blob has <= cap
  elements and is single-kind.
- `repack_blob_count_matches_prediction` - per-kind output blob
  count matches `ceil(elements / cap)` on a clean shrink.
- `repack_propagates_sorted_flag` - `Sort.Type_then_ID` round-trips.
- `repack_rejects_zero_cap` - `--elements-per-blob 0` fails up front.
- `repack_grow_collapses_to_one_blob_per_kind` - cap >> all per-kind
  totals (60/12/3 vs cap 8000): one output blob per kind, "never
  fired" warning fires.
- `repack_round_trip_preserves_elements_on_grow` - element/ID/tag
  multiset equality after a grow that fires the cap mid-stream
  (cap=30 vs input 20/blob).
- `repack_grow_blob_count_matches_prediction` - cap=30 grows produce
  `ceil(60/30)=2` node blobs, exercising cross-input-blob coalesce.
- `repack_no_warning_when_cap_fires` - regression sentinel against
  false-positive warnings on real shrinks.
- `repack_grow_no_warning_when_cap_fires` - same sentinel for grows
  that fire the cap mid-stream.

Cross-validation against osmium's `cat` re-block flag is **not** in
the suite; deferred to v2.3.

## v2 scope

v2.1 (cross-input-blob coalescing) shipped. Two gaps remain; v2.2 is
the next likely candidate, v2.3 is cheap once it lands.

### v2.2 - LocationsOnWays preservation

**Goal:** if the input header declares `LocationsOnWays`, the
output preserves both the feature flag and the inline coordinates
on way refs.

**Constraint:** no implicit conversion. Input without LOW must
produce output without LOW; input with LOW must produce output with
LOW. There is no `--add-locations-to-ways` mode here - that is what
the existing `add-locations-to-ways` command is for.

**Sketch:**

`Element::Way` already exposes inline coordinates when the source
blob has them. The writer path - `BlockBuilder::add_way` - takes
only `refs: &[i64]` today. v2.2 needs:

1. A second `BlockBuilder` entry point (`add_way_with_locations` or
   an enum-valued variant) that takes `refs` plus parallel
   `decimicro_lat` / `decimicro_lon` slices.
2. The repack worker, when the input header has LOW, populates
   those slices from the way's inline coordinates.
3. The output header inherits the LOW feature flag instead of being
   stripped by `warn_locations_on_ways_loss`.

Then drop the `warn_locations_on_ways_loss` call from `repack` (the
warning still belongs in commands that genuinely cannot preserve
LOW, like `tags-filter` and `extract`).

**Tests to add:**

- `repack_preserves_locations_on_ways` - fixture with LOW + a
  handful of ways, verify the output declares LOW and that
  way-ref coordinates round-trip exactly.
- `repack_strips_no_low_when_input_has_none` - sentinel that we
  don't accidentally emit LOW when the input doesn't have it.

### v2.3 - osmium cross-validation

**Goal:** add a `brokkr verify repack` cross-check against osmium
in the standard verify suite.

**Constraint:** osmium's re-block flag (verify the exact name in
`osmium cat --help`) must produce a comparable output. If it
doesn't, the cross-check is element-equality only, not byte-
equality.

Cheap to add now that v2.1 has landed; pick up when a measurement
run actually needs the third-party sanity check.

## Out of scope (still deferred past v2)

- `--blob-size-bytes N` - target compressed bytes instead of
  element count. Harder to target precisely; requires a
  feedback-loop sizing strategy in the coalescer.
- `--densify` / `--undensify` - convert between DenseNodes and
  plain Node encoding. Useful for measurement but a different
  surface; arguably belongs in `degrade` rather than `repack`.
- `--normalize-compression zlib:6` - force a canonical re-encode
  pass. Easy lift; do it when a measurement run actually needs it.
- Long-run progress feedback (like `apply-changes`). Useful for
  planet repacks; low priority.

## Cross-references

- [`reference/blob-density.md`](../reference/blob-density.md) - the
  insight that motivates this command.
- [`notes/getparents.md`](getparents.md) - the original blocking
  consumer (HeaderWalker dispatch threshold). v1's shrink path
  unblocks the planet measurement; v2.1's grow path unblocks the
  Geofabrik measurement.
- [`notes/degrade.md`](degrade.md) - companion command for
  adversarial testing; shares the "take a PBF, emit a derived
  PBF" pipeline shape.
- [`src/commands/repack/mod.rs`](../src/commands/repack/mod.rs) -
  the implementation.
- [`src/write/block_builder.rs`](../src/write/block_builder.rs) -
  `BlockBuilder::with_element_cap`, where the cap lives.
- [`src/commands/cat/mod.rs`](../src/commands/cat/mod.rs) - the
  parallel three-phase pipeline that `repack` mirrors.
