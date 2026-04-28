# `repack` - command status and v2 scope

Re-encode a PBF with a configurable element-count cap per blob.
Motivated by [`reference/blob-density.md`](../reference/blob-density.md):
the measurement matrix needs same-corpus-different-encoding pairs to
control for blob-count effects independent of byte size.

**Status:** v1 shipped. This document records what v1 actually does
(it diverged from the original sketch) and what v2 still needs.

## What v1 does

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
ways, then relations), mirroring `cat --clean`. Each worker decodes
one input blob, re-encodes its matching elements through a
`BlockBuilder` configured with the requested cap, and emits the
resulting framed blob bytes. Output is streamed in input-seq order
via a `ReorderBuffer`, so peak RSS is bounded by the in-flight worker
count rather than total output size.

This shape replaced the original sequential `for_each_element`
sketch. The parallel classify pipeline already existed for
`cat --clean` and gives us free per-kind segregation, the right
peak-RSS profile, and the same fault-injection coverage as the rest
of the parallel-write commands.

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
`repack_blobs_written` and `repack_elements_written` (visible via
`brokkr sidecar <uuid>`).

### v1 limitation: per-worker cap, no cross-input-blob coalescing

The cap fires per worker invocation, so output blobs cannot grow
beyond the input blob size:

- **Shrink** (planet's ~228 k/blob -> 8 k/blob): one input blob
  produces multiple output blobs and the cap fires correctly. This
  is the blob-density measurement use case and works as designed.
- **Grow** (Geofabrik 8 k/blob -> 64 k/blob): cross-input-blob
  coalescing is needed and is **not** implemented.

When a grow attempt produces no actual repacking (cap exceeds every
input blob), the run prints a stderr warning so the silent-identity
outcome is visible:

```
Warning: --elements-per-blob N never fired; cap exceeds the largest
input blob, so the output blob layout matches the input. v1 cannot
grow blobs across input-blob boundaries (deferred to v2).
```

### v1 limitation: LocationsOnWays not preserved

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
- `repack_large_cap_preserves_input_blob_layout` - grow attempt
  prints the "never fired" warning and the output is element-equal
  with the same input-blob layout.
- `repack_no_warning_when_cap_fires` - regression sentinel against
  false-positive warnings on real shrinks.

Cross-validation against osmium's `cat` re-block flag is **not** in
the suite; deferred to v2 alongside the cross-input-blob work.

## v2 scope

Two known v1 gaps. Pick them off in order; the grow path is the one
blocking real measurement work.

### v2.1 - cross-input-blob coalescing (the grow path)

**Goal:** make `--elements-per-blob N` fire correctly when N exceeds
the input blob size, so Geofabrik 8 k/blob -> planet-style 256 k/blob
re-packings produce the requested output.

**Constraint:** preserve the parallel three-phase pipeline shape so
peak RSS stays bounded and the per-kind segregation remains free.

**Sketch:**

A worker today: one input blob -> 1+ framed output blob(s),
emitted in seq order via `ReorderBuffer`. The cap fires inside the
worker and the worker is the framing boundary.

For grow, the framing boundary has to move past the input-blob
boundary. Two plausible shapes:

1. **Per-kind serial coalescer downstream of the worker pool.**
   Workers stop framing; they emit decoded `OwnedBlock`s in seq
   order. A single coalescer thread per kind feeds those into one
   long-lived `BlockBuilder`, flushing to a framed blob each time
   the cap fires. Compression/framing happens on the coalescer
   thread (or on a fan-out write pool downstream).

   Trade-off: the coalescer is a serial choke point. For shrink it
   does no work (the worker already produced cap-sized blobs); for
   grow it does all the framing work. Compression-bound on planet
   it could become the bottleneck.

2. **Worker emits decoded elements in seq order; downstream block
   layout is computed deterministically.**
   The cap point is decided by element index (`element_seq /
   cap`). Workers can pre-frame any complete output blob whose
   element range falls entirely within their input blob; cross-
   boundary blobs are framed by a small downstream task that joins
   the trailing elements of input blob `k` with the leading
   elements of input blob `k+1`.

   Trade-off: more book-keeping, but the slow framing path stays
   parallel. The element-index addressing also lets us emit a
   deterministic `--bench` artifact.

Pick (1) for v2.1 unless profiling shows the coalescer is the
bottleneck. The simpler shape is worth the risk; if it lands and
benchmarks show the choke, (2) is a v2.1.x follow-up.

**New behavior:**

- The "never fired" warning becomes much rarer: only when the input
  has fewer than `cap` elements total per kind.
- A new counter `repack_input_blobs_coalesced` (or similar)
  measures how often the coalescer crosses an input-blob boundary,
  visible via `brokkr sidecar`.

**Tests to add:**

- `repack_round_trip_preserves_elements_on_grow` - shrink fixture
  with cap larger than per-blob count, verify element/ID/tag
  multiset equality and that the output blob count drops.
- `repack_grow_blob_count_matches_prediction` - 60 nodes packed
  20/blob, cap=64, expect 1 output node blob.
- `repack_grow_no_warning_when_cap_fires` - regression sentinel.
- The existing `repack_large_cap_preserves_input_blob_layout` test
  needs to either be deleted or rewritten to assert that the
  warning *does not* fire and the output is one blob per kind.

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

Cheap once v2.1 lands. Skip until then.

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
