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

Note on the `_matches_prediction` tests: they assert `ceil(elements /
cap)` because their fixtures divide evenly (cap 10 vs 20/blob input ->
zero tails -> guard never fires). That identity is not general - see
"Output ordering guard and its density trade" above.

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
