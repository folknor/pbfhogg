# `repack` - command status and v2 scope

Re-encode a PBF with a configurable element-count cap per blob.
Motivated by [`reference/blob-density.md`](../reference/blob-density.md):
the measurement matrix needs same-corpus-different-encoding pairs to
control for blob-count effects independent of byte size.

**Status:** v1 + v2.1 + v2.2 shipped. v1 was the parallel per-worker
shrink path; v2.1 added cross-input-blob coalescing so grow caps
actually fire; v2.2 preserves LocationsOnWays (the input's inline
way-ref coordinates and feature flag round-trip exactly). v2.3
(osmium cross-validation) remains deferred.

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
seq order via a `ReorderBuffer`. For each input blob, when that blob
carries direct full blocks it first drains the central stream (flush
the builder, write everything buffered) so the earlier lower-ID tails
precede this blob's higher-ID full blocks, then writes the worker's
full framed blocks directly, then feeds the trailing `Owned*` slice
into the central builder; mid-stream flushes are framed in parallel
via `rayon::par_iter` over `FRAME_BATCH`-sized batches and written
serially.

This hybrid keeps shrinks fast (when `cap` divides `M` cleanly all
work happens in workers, matching v1) while supporting grows
correctly: when `cap > M_per_input_blob` each worker emits 0 full
blocks and ships everything as trailing, and the central builder
spans input-blob boundaries to produce cap-sized output.

### Output ordering guard and its density trade

The two output streams - worker full blocks written directly, and the
central builder's coalesced tail blocks flushed in `FRAME_BATCH`
batches - are written to one file. Because the input is
`Sort.Type_then_ID` sorted, an input blob's low `M - M%cap` IDs land
in its direct full blocks and its high `M%cap` IDs land in the tail,
so blob N's full blocks outrank blob N-1's tail. Writing the direct
full blocks immediately while the tail sat in a delayed batch let a
later blob's higher-ID full block precede an earlier blob's lower-ID
tail, producing output that violated the `Sort.Type_then_ID` its own
header still advertised (confirmed 2026-07-12 on planet 8k: 7
non-monotonic relation violations). The fix: before writing any input
blob's direct full blocks, drain the central stream (flush the builder,
write all buffered blocks) so the earlier lower-ID tails go out first.

**Accepted density change.** The guard flushes each input blob's tail
as its own block instead of packing tails across input-blob
boundaries. So on a coalescing shrink (cap smaller than the
per-input-blob element count) the output blob count is NO LONGER the
general `ceil(elements / cap)`: it now depends on the input-blob
boundaries, because each input blob whose element count is not a
multiple of the cap contributes an extra possibly-under-cap tail
block, and parallel framing runs on smaller batches. This is a
deliberate trade - coalescing in the shrink/mixed case was buying
little and was the exact source of the reordering. `ceil(elements /
cap)` still holds only when the cap divides the per-input-blob element
count (zero tails, guard never fires) or when a kind occupies a single
input blob (its tail cannot cross a boundary). Pure grows (empty
`full_framed`) never trip the guard, so their cross-input-blob
coalescing is unchanged.

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

Re-validated 2026-07-10 at `8c1cf03` (plantasjen): planet 8k bench
**389.6 s / 1.31 GB peak anon** (UUID `a4791ddc`) - consistent with
the April number. Output: 1,453,433 blobs (28.6x the 50,816 input
blobs), 98.4 GB; writer `recv_wait` 339 s of 390 s wall, so
worker-side compression is the ceiling.

Bench-mode peak RSS stays under 1.5 GB even at planet scale - the
per-kind parallel-classify pipeline is bounded per worker, so the
working set does not grow with the input.

**Artifact preservation (resolved 2026-07-10):** the first two planet
8k runs kept no output - bench mode writes to scratch
(`bench-repack-output.osm.pbf`), overwritten per iteration and swept
by `brokkr clean` - so the "artefact that unblocks blob-density" never
actually survived them. Resolved by a third run with snapshot
promotion: UUID `8027765b` (377.5 s, commit `8c1cf03`, plantasjen)
promoted its output to `data/planet-8k-with-indexdata.osm.pbf` and
registered it as `[datasets.planet.snapshot.8k]` `pbf.indexed`. That
file is the input for
[`reference/blob-density.md`](../reference/blob-density.md) and the
deferred `HeaderWalker` dispatch decision in
[`notes/getparents.md`](getparents.md); consumer commands reach it via
`--snapshot 8k` (consumer-side snapshot support is complete as of
brokkr `e635f5b` - see AGENTS.md).

### Profiler-mode OOM (fixed at commit `195b7ff`)

Historical: `brokkr repack --hotpath` / `--alloc` were OOM-killed at
europe and planet scale during the 2026-04-28 overnight run, with
peak anon-rss 28.9-29.1 GB on a 30 GB host. Bench mode at the same
dataset and cap landed at <1.5 GB, so the OOM scaled with profiler
overhead, not the repack pipeline itself.

Root cause: `BlockBuilder::add_node` / `add_way` /
`add_way_with_locations` / `add_relation` carried unconditional
`#[hotpath::measure]` annotations. At europe (700M elements) /
planet (9B+ elements) the per-element span recording exhausted the
hotpath span buffer. Phase boundaries (`take`, `take_owned`, the
encode helpers) keep their annotations - they fire per-block, not
per-element, so they don't scale with input size.

Verified at europe (commit `195b7ff`, plantasjen):

| Mode      | Wall   | UUID       |
|-----------|--------|------------|
| `--hotpath` | 156 s | `eff071a3` |
| `--alloc`   | 157 s | `81fb1aaf` |

Both complete with informative function tables. Planet re-measure
deferred but no longer expected to OOM.

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

### LocationsOnWays preservation (v2.2)

No implicit conversion: the output mirrors the input's LOW state.

- Input header WITHOUT `LocationsOnWays`: output carries no LOW
  feature flag and no inline way coordinates (unchanged pre-v2.2
  shape). There is no add-locations mode here - that is what the
  `add-locations-to-ways` command is for.
- Input header WITH `LocationsOnWays`: the output header re-advertises
  the feature and every way-ref `(decimicro_lat, decimicro_lon)`
  round-trips exactly.

Detection is a single up-front `HeaderBlock::has_locations_on_ways`
read. When active, both way-phase output streams carry coordinates:
the per-worker `BlockBuilder` calls `add_way_with_locations` on the
full-block path, and the trailing slice ships as `OwnedWay` extended
with the per-ref coordinates so the central merge builder also uses
`add_way_with_locations`. A way that carries no inline coordinates
under a LOW header (empty lat/lon fields) falls back to the plain
`add_way` encoding for that way, so the refs==locations invariant
never trips. The shared `warn_locations_on_ways_loss` call was
dropped from `repack` - it stays in commands that genuinely cannot
preserve LOW (tags-filter, extract, sort, and so on) - and replaced
with a narrower `warn_way_metadata_loss` local to the repack module.
repack still drops the two `pbfhogg.*` prepass features
(`WayMembers-v1`, `SharedNodePins-v1`; see "What repack does not
preserve" below), so the narrowed warning still fires for those two,
just never for `LocationsOnWays`.

### What v1 preserves

- Element IDs, tags, and OsmMetadata (version, timestamp, changeset,
  uid, user, visible) for every kind.
- Way refs (delta-encoded node IDs).
- Relation members (id + type + role).
- DenseNode encoding (DenseNode in -> DenseNode out).
- `Sort.Type_then_ID` header flag when the input has it.
- `LocationsOnWays` feature flag and inline way-ref coordinates when
  the input has them (v2.2).
- All `OsmSchema-V0.6` / `DenseNodes` / `HistoricalInformation`
  features that pass through the standard writer header path.

### What repack does not preserve

repack still drops the two `pbfhogg.*` injected prepass metadata
features: `pbfhogg.WayMembers-v1` (BlobHeader field-5 way-member
bitmaps) and `pbfhogg.SharedNodePins-v1` (Way field-20 shared-node
pin bitmaps). An input header declaring either trips
`warn_way_metadata_loss` (repack-local, narrower than the shared
`warn_locations_on_ways_loss`), which does not fire for
`LocationsOnWays` since v2.2.

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
- `repack_output_is_monotonic_across_coalesced_blob_boundaries` -
  ordering-guard regression: 40 relations across two 20-element input
  blobs, cap 12, walks output-order relation IDs and asserts strictly
  increasing. Fails pre-fix (ID 13 after ID 32), passes post-fix.
- `repack_output_is_monotonic_for_nodes_and_ways` - same regression
  extended to nodes (dense) and ways, which run the same two-stream
  merge path: 40 of each across two 20-element blobs, cap 12.
- `repack_output_is_monotonic_with_pending_prepopulated_at_guard` -
  fires the guard while the central `pending` buffer is already
  non-empty (relation blobs 5, 5, 5, 15 so three all-tail blobs queue a
  coalesced block before a full-block-bearing blob arrives), covering
  the path the base regression never reaches. The strict
  `bb`-empty-`pending`-non-empty guard state is unreachable (lazy
  builder flush always leaves `bb` a non-empty remainder), so its
  disjunct is defensive.
- `repack_preserves_locations_on_ways` - LOW fixture (header declares
  `LocationsOnWays`, 5 ways with inline coords, one node blob), cap 2
  so the way phase splits into worker full blocks plus a trailing
  merge slice; asserts the output header declares LOW and every
  way-ref coordinate round-trips exactly across both paths.
- `repack_strips_no_low_when_input_has_none` - sentinel for the
  no-implicit-conversion constraint: the standard (no-LOW) fixture
  produces output with no LOW feature and no inline way coordinates.
- `repack_strips_coords_when_input_flag_absent` - reverse direction of
  the above: ways carry real inline coordinates in the wire data but
  the header omits `LocationsOnWays`; asserts the gate is
  `header.has_locations_on_ways()` alone, not the presence of
  coordinate fields, so the output declares no LOW and every way's
  `node_locations()` is empty.

Note on the `_matches_prediction` tests: they assert `ceil(elements /
cap)` because their fixtures divide evenly (cap 10 vs 20/blob input ->
zero tails -> guard never fires). That identity is not general - see
"Output ordering guard and its density trade" above.

Cross-validation against osmium's `cat` re-block flag is **not** in
the suite; deferred to v2.3.

## v2 scope

v2.1 (cross-input-blob coalescing) and v2.2 (LocationsOnWays
preservation) shipped. One gap remains; v2.3 is cheap now that v2.2
has landed.

### v2.2 - LocationsOnWays preservation (shipped)

Shipped. Detection, the two coordinate-carrying way-phase paths, and
the header re-advertisement are described under "LocationsOnWays
preservation (v2.2)" above; `repack_preserves_locations_on_ways` and
`repack_strips_no_low_when_input_has_none` cover it. The `OwnedWay`
owned type gained a `locations: Vec<(i32, i32)>` field (empty on the
`read_way` path so sort / cat-dedupe / degrade / time_filter behave
unchanged; populated only by the new `read_way_with_locations`).

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
