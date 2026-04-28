# `degrade` - command status and v2 scope

Produce a valid-but-adversarial PBF by stripping properties or
perturbing structure. A "make our lives difficult" tool for exercising
code paths that require less-optimised inputs (unsorted, missing
indexdata, scattered coords).

**Status:** v1 shipped. This document records what v1 actually does
and what v2 still needs.

## What v1 does

Code: [`src/commands/degrade/mod.rs`](../src/commands/degrade/mod.rs);
CLI: `pbfhogg degrade`.

```
pbfhogg degrade <input> -o <output> [--unsort]
                                     [--strip-locations]
                                     [--strip-indexdata]
                                     [--compression C]
                                     [--direct-io] [--io-uring]
                                     [--generator ...]
                                     [--output-header ...]
```

Flags compose; at least one transformation flag is required. The CLI
also exposes a hidden `--block-cap N` so the test suite can exercise
the `--unsort` swap on small fixtures (production runs use the default
8000).

### Implementation paths

The implementation picks one of two paths up front based on the flag
combination:

- **Pure passthrough** (`--strip-indexdata` alone): raw blob frames
  are iterated via `read_raw_frame`, the `BlobHeader.indexdata` field
  is cleared, the rest of the frame (compressed Blob payload + any
  `tagdata`) is forwarded verbatim. Sortedness, `LocationsOnWays`
  inline coordinates, and every element-level property pass through
  unchanged because the blob bytes are not touched.
- **Decode path** (any flag set involving `--unsort` or
  `--strip-locations`): elements stream through `into_blocks_pipelined`,
  are re-emitted via `BlockBuilder`, and framed with `frame_blob_pipelined`.
  When `--strip-indexdata` composes, the framing path is taken with
  `indexdata=None`; the standard `write_primitive_block_owned` path
  embeds indexdata and is bypassed for this combination.

The choice is structural: cross-input-blob element reordering (the
`--unsort` perturbation) cannot be done at the blob-passthrough level,
and `--strip-locations` requires re-encoding ways without inline
coords.

### Transformations

#### `--unsort`

Clears `Sort.Type_then_ID` from the output header. Perturbs the
element stream so adjacent same-kind output blobs have overlapping ID
ranges, exactly one such pair per kind that has more than `block_cap +
1` elements:

- Hold the `(block_cap - 1)`-th element of each kind (1-indexed: the
  cap-th element to arrive).
- After the `block_cap`-th element fills out the previous block,
  re-inject the held element as the first element of the next block.
- Result: previous block's `max_id` is the cap-th element's id;
  next block's `min_id` is the held (cap-1)-th element's id, which is
  smaller. `sort`'s `detect_overlaps` flags the boundary because
  `max_id_prev >= min_id_next`.

This is the minimum perturbation that gets `sort` to dispatch to the
overlap-rewrite path without chaos-ifying the file. The two output
blocks are still internally ID-monotone; only the inter-blob ordering
breaks.

#### `--strip-locations`

Clears the `LocationsOnWays` optional feature from the output header
and re-encodes ways via `BlockBuilder::add_way` (without coordinates),
so inline way-node coordinates do not survive the round-trip. Other
element data round-trips normally. The standard
`warn_locations_on_ways_loss` warning is suppressed for this flag
because the loss is the explicit goal.

#### `--strip-indexdata`

Clears the `BlobHeader.indexdata` field on every OsmData blob.
`tagdata` is preserved (`--strip-tagdata` is a separate, deferred
flag). On the passthrough path the blob payload is not decompressed;
on the decode path the framing call passes `indexdata=None`.

### What v1 preserves

On the passthrough path: every blob payload byte (including LOW
inline coords), all element-level properties, the `Sort.Type_then_ID`
header flag, and `LocationsOnWays` when the input declared them.

On the decode path: element IDs, tags, refs, members, OsmMetadata,
DenseNode encoding. `Sort.Type_then_ID` is preserved unless `--unsort`
clears it. `LocationsOnWays` is dropped by `BlockBuilder` as on every
other decode-path command in pbfhogg (`repack`, `sort`, `tags-filter`,
etc.); `--strip-locations` makes the loss explicit.

### Tests

[`tests/cli_degrade.rs`](../tests/cli_degrade.rs):

- `degrade_strip_indexdata_drops_indexdata` - output has no
  `BlobHeader.indexdata` on the first OsmData blob; sortedness
  preserved; element multiset round-trips.
- `degrade_strip_locations_clears_low_and_preserves_elements` -
  output header has no `LocationsOnWays`; element data round-trips.
- `degrade_unsort_creates_adjacent_overlap_per_kind` - output header
  has no `Sort.Type_then_ID`; for each kind at least one adjacent
  same-kind blob pair has overlapping ID ranges; element multiset
  round-trips.
- `degrade_unsort_then_sort_round_trips` - the design's primary
  consumer loop: `degrade --unsort` then `pbfhogg sort` recovers
  the original element set with `Sort.Type_then_ID` re-declared.
- `degrade_unsort_and_strip_indexdata_compose` - composition test:
  output is unsorted *and* unindexed.
- `degrade_requires_at_least_one_flag` - validation: no flags is a
  hard error.
- `degrade_rejects_zero_block_cap` - validation: `--block-cap 0`
  is a hard error.

Combination matrix beyond the explicit composition test is implicit:
the decode path's flush is routed through a single helper that
respects `--strip-indexdata`, so any pairing of decode-path flags
gets the right framing.

## v2 scope

The deferred transformations from the design doc, ordered by likely
demand. Pick them up as benchmarking work surfaces consumers.

### v2.1 - `--strip-tagdata`

**Goal:** clear the per-blob tagdata index (`BlobHeader` field 4) on
OsmData blobs, forcing `tags-filter`'s no-hint fallback path.

**Sketch:** mirrors `--strip-indexdata` exactly. On the passthrough
path, the existing `reframe_raw_without_index` already passes tagdata
through; a sibling helper would pass `tagdata=None`. On the decode
path, the framing call already takes a tagdata argument; default it
to `None` when the flag is set. Composes with everything.

**Tests:** `--strip-tagdata` clears tagdata on every blob;
`tags-filter` on the degraded output still produces correct results,
just slower (hits the no-hint path).

### v2.2 - `--strip-bbox`

**Goal:** clear the HeaderBlock bbox so `extract`'s spatial-scan
fallback fires.

**Sketch:** header-only transformation. Builds the output header via
`HeaderBuilder::from_header` minus the `bbox` field. Composes with
everything. No element-level work.

**Tests:** output header has no bbox; `extract --bbox` on the
degraded output still produces correct results.

### v2.3 - `--recompress C`

**Goal:** re-encode at a different compression codec without changing
blob size. Distinct from `--compression`, which controls the output
codec for the whole run; `--recompress` would force a decompress +
recompress pass even when the rest of the run is a passthrough.

**Sketch:** when set, the passthrough path is disabled even for
`--strip-indexdata` alone - we have to decompress to pick up the new
codec. This is essentially "decode + re-encode without changing
structure". Overlaps with `repack --compression`, which already does
this when the cap matches the input.

**Tests:** input zlib + `--recompress none` -> output is uncompressed;
element multiset round-trips.

### v2.4 - `--drop-ids N:SEED`

**Goal:** introduce referential dangles for `check --refs` slow-path
benchmarking and error-recovery validation.

**Sketch:** during the decode-path pass, deterministically drop N
elements (chosen by hashing id with seed) from the output. Ways and
relations referencing the dropped IDs become dangling. The
`--strip-indexdata` passthrough path is unsuitable here - dropping
elements changes blob counts, so we must decode.

**Tests:** output has exactly the original count minus N elements;
`check --refs` on the degraded output reports the expected number of
dangling references, and the seed is reproducible.

### v2.5 - `--unsort` chaos modes

**Goal:** richer perturbation patterns beyond the v1 minimum-viable
swap. Candidates: `--unsort=rotate` (rotate each kind's element stream
by `block_cap` so every output blob is non-monotone), `--unsort=shuffle`
(deterministic-seeded full shuffle), `--unsort=reverse` (reverse the
element stream).

**Constraint:** v1's swap is intentionally surgical. Adding modes is
useful for stressing `sort` at scale but is not blocking any specific
benchmark. Land when `sort` opp-3 measurement work needs more than
"detect_overlaps fires once".

### v2.6 - `--unsort` planet pre-generation

**Goal:** register a `degrade --unsort planet` artefact in
`brokkr.toml` so `brokkr sort --dataset planet-unsorted --bench 1`
can run without regenerating the input each time.

**Cheap once we decide the chaos mode** for planet measurements -
v1's surgical swap may be sufficient (one overlap per kind = one
overlap-rewrite span per kind = directly measures the rewrite path).
Bigger chaos modes burn more wall on the generator side.

## Out of scope

- Element filtering (use `getid`, `tags-filter`, or `extract`).
- Blob-size re-encoding (use `repack`).
- Format conversion (XML / OPL output is a separate tool).

## Cross-references

- [`reference/blob-density.md`](../reference/blob-density.md) - the
  parallel insight; `degrade` is orthogonal but shares the
  "generate adversarial test data" framing.
- [`notes/repack.md`](repack.md) - companion command; shares input-
  read + output-write plumbing and the same v1/v2 split.
- [`notes/sort.md`](sort.md) - primary consumer of `--unsort` for
  the overlap-rewrite path measurement.
- [`src/commands/degrade/mod.rs`](../src/commands/degrade/mod.rs) -
  the implementation.
- [`src/commands/sort/mod.rs`](../src/commands/sort/mod.rs) -
  `detect_overlaps` is the function `--unsort` is designed to
  trigger.
- [`src/write/block_builder.rs`](../src/write/block_builder.rs) -
  `BlockBuilder`; element-level transformations re-emit through it.
- [`src/write/framing.rs`](../src/write/framing.rs) -
  `frame_blob_pipelined` and `encode_blob_header_into`; the
  decode-path flush goes through these directly when
  `--strip-indexdata` composes, bypassing the indexdata-embedding
  `write_primitive_block_owned` path.
