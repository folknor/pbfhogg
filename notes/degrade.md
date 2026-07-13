# `degrade` - command status and v2 scope

Produce a valid-but-adversarial PBF by stripping properties or
perturbing structure. A "make our lives difficult" tool for exercising
code paths that require less-optimised inputs (unsorted, missing
indexdata, scattered coords).

**Status:** v1 shipped, planet-scale safe in every transformation
mode after the per-kind classify-pipeline port. This document
records what v1 actually does and what v2 still needs.

## What v1 does

Code: [`src/commands/degrade/mod.rs`](../src/commands/degrade/mod.rs);
CLI: `pbfhogg degrade`.

```
pbfhogg degrade <input> -o <output> [--unsort]
                                     [--unsort-intra]
                                     [--strip-locations]
                                     [--strip-indexdata]
                                     [--strip-tagdata]
                                     [--strip-bbox]
                                     [--drop-ids N:SEED]
                                     [--force]
                                     [--compression C]
                                     [--direct-io] [--io-uring]
                                     [--generator ...]
                                     [--output-header ...]
```

Flags compose, except `--unsort` and `--unsort-intra`, which are
mutually exclusive (they request opposite blob shapes); at least one
transformation flag is required. The CLI also exposes a hidden
`--block-cap N` so the test suite can exercise the unsort swaps on
small fixtures (production runs use the default 8000). `--unsort-intra`
requires `--block-cap >= 2` (a cap of 1 cannot hold two same-kind
elements in one output block, so the intra-blob inversion is
impossible); `--unsort` accepts any `--block-cap >= 1`. `--force`
skips the indexdata precondition required by the decode path's
per-kind classify pipeline (without indexdata, every classify schedule
includes every blob, so each blob is decoded by each phase).

### Implementation paths

The implementation picks one of two paths up front based on the flag
combination:

- **Pure passthrough** (`--strip-indexdata`, `--strip-tagdata`, and/or
  `--strip-bbox`, with no unsort/strip-locations/drop-ids): this is a
  header-and-blob passthrough, split into two independent surgical
  strips that share one wire-level helper
  (`strip_message_fields` in `src/write/framing.rs`, exposed as
  `strip_blob_header_fields` for the per-blob `BlobHeader` and
  `strip_header_block_fields` for the file-level `HeaderBlock`):
  - *OSMHeader*: with no `--generator`/`--output-header` override, the
    input `HeaderBlock` payload (the decompressed OSMHeader blob body)
    is forwarded field-for-field via `strip_header_block_fields`, with
    only the bbox (field 1) removed under `--strip-bbox` -
    `passthrough_header_bytes`' verbatim branch. When an override *is*
    present, `passthrough_header_bytes` instead rebuilds the header
    through `HeaderBuilder::from_header` (bbox omitted under
    `--strip-bbox`) so the override can apply; this rebuild path is a
    lossy one (see "What v1 preserves" below) and only fires when the
    user explicitly asked for a header rewrite.
  - *OsmData blobs*: raw blob frames are iterated via `read_raw_frame`,
    and each output `BlobHeader` is the *original* header copied
    through field-by-field with only the targeted hint field(s) omitted
    - `indexdata` (field 2) under `--strip-indexdata`, `tagdata` (field
    4) under `--strip-tagdata`. `--strip-bbox` alone never touches an
    OsmData `BlobHeader` or payload - it is entirely a HeaderBlock
    change.

  This surgical wire-level omission preserves every other targeted
  message's field byte-for-byte: the untargeted indexdata hint keeps
  its exact original bytes (a 26-byte v1 index stays v1, never upgraded
  to the 42-byte v2 layout, and an index that fails to deserialize is
  kept rather than dropped), and `pbfhogg.WayMembers-v1` (field 5) plus
  any unknown/extension fields survive untouched, on both the
  `BlobHeader` and the `HeaderBlock`. The compressed Blob payload is
  copied byte-for-byte, so the preserved `datasize` (field 3) stays
  consistent. Sortedness, `LocationsOnWays` inline coordinates, and
  every element-level property pass through unchanged because the blob
  bytes are not touched. Accepts non-indexed input (no precondition).
  `--strip-tagdata` alone therefore yields a still-sorted, still-indexed
  file that only lacks the per-blob tag key index; `--strip-bbox` alone
  yields an otherwise-untouched file with no declared file-level
  extent.
- **Decode path** (any flag set involving `--unsort`, `--strip-locations`,
  or `--drop-ids`; `--strip-indexdata` / `--strip-tagdata` ride
  along as `indexdata=None` / `tagdata=None` on the framing call;
  `--strip-bbox` rides along by omitting the bbox from the rebuilt
  output header): three sequential per-kind phases driven by
  [`parallel_classify_phase`](../src/scan/classify.rs), mirroring
  [`repack`](../src/commands/repack/mod.rs). For each kind in
  `nodes -> ways -> relations`: workers decode one input blob,
  filter to the current kind, and (when not `--unsort`) pre-frame
  full cap-multiples through a per-worker `BlockBuilder`. The merge
  thread runs a single long-lived `BlockBuilder` per kind, flushes
  it before writing each input blob's worker frames (sort
  preservation), and applies the `--unsort` cap-1 swap state
  machine to the trailing slice. `--strip-indexdata` composes by
  passing `indexdata=None` to `frame_blob_pipelined`; the standard
  `write_primitive_block_owned` path that embeds indexdata is
  bypassed for the decode path. Requires indexdata (or `--force`).

The choice is structural: cross-input-blob element reordering (the
`--unsort` perturbation) cannot be done at the blob-passthrough level,
`--strip-locations` requires re-encoding ways without inline
coords, and `--drop-ids` changes per-blob element counts (and
therefore blob framing), which a byte-for-byte blob copy cannot do.

### Decode-path memory model

The per-kind phasing bounds working set the same way `repack` does:

- Only one kind's blobs are scheduled at a time. Mixed-kind input
  blobs are decoded once per phase that includes them.
- The reorder buffer caps at 32 worker outputs in flight per phase.
- Worker pre-framed blocks are bounded by the input blob's element
  count divided by `block_cap` (typically a few MB of compressed
  bytes per worker output).
- Under `--unsort` the worker output is `Owned*` data instead of
  pre-framed blocks - up to the input blob's full kind count per
  slot. At planet scale a node blob holds ~228k nodes, so 32 slots
  is ~350 MB worst case, still well under the 1.5 GB ceiling that
  `repack` peaks at.

### Decode-path throughput model

Two distinct shapes:

- `--strip-locations` (no `--unsort`): workers re-encode in
  parallel; merge thread writes worker frames directly and only
  encodes the trailing `M%cap` slice per input blob. Same shape as
  `repack`'s shrink path.
- `--unsort` (with or without `--strip-locations`): workers ship
  every matching element as `Owned*`; the merge thread runs all
  encoding serially because the cap-1 swap needs to see elements
  in stream order. Throughput is bounded by the single-thread
  encoder. Acceptable because `--unsort` is a one-time generation
  step for `sort` benchmarking, not a hot path. See [v2.7](#v27-
  --unsort-throughput-recovery) for sketches that recover parallel
  re-encode if a hot-loop consumer ever surfaces.

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
breaks. Achievable for any `block_cap >= 1` (at cap 1 the two adjacent
single-element blobs overlap directly).

#### `--unsort-intra`

Clears `Sort.Type_then_ID` from the output header. Produces the
opposite adversarial shape from `--unsort`: exactly one same-kind blob
per kind carries an internal ID-order inversion, but every blob's ID
range stays disjoint from its neighbours'. `sort`'s `detect_overlaps`
(which compares adjacent blobs' `(min_id, max_id)` ranges) therefore
sees nothing to fix even though the stream is genuinely out of order -
the intra-blob monotonicity blind spot.

- Swap the first two same-kind elements to arrive (hold element #1,
  re-inject it after element #2). Both land at the start of the first
  output block (positions 1 and 2), so the descending step sits well
  inside a blob, away from any block boundary.
- Because the swap is keyed to the start of the stream rather than the
  cap boundary, it stays intra-blob for any `block_cap >= 2`,
  independent of where input- or output-blob boundaries fall - in
  particular even when a single input blob carries more than
  `block_cap` same-kind elements. (Keying the swap to the cap boundary,
  the way `--unsort` does, would fill and flush an output block there
  and produce the cross-blob shape instead; that was the pre-fix bug.)

Requires `block_cap >= 2`: a cap of 1 puts every element in its own
blob, so no blob can hold the two elements an intra-blob inversion
needs. `degrade` rejects `--unsort-intra --block-cap 1` up front rather
than clearing `Sort.Type_then_ID` and emitting an untouched stream.

#### `--strip-locations`

Clears the `LocationsOnWays` optional feature from the output header
and re-encodes ways via `BlockBuilder::add_way` (without coordinates),
so inline way-node coordinates do not survive the round-trip. Other
element data round-trips normally. The standard
`warn_locations_on_ways_loss` warning is suppressed for this flag
because the loss is the explicit goal.

#### `--strip-indexdata`

Clears the `BlobHeader.indexdata` field (field 2) on every OsmData
blob. `tagdata` is preserved unless `--strip-tagdata` is also set. On
the passthrough path the blob payload is not decompressed and every
other header field (including `tagdata`, `WayMembers-v1`, and any
unknown fields) is copied through byte-for-byte; on the decode path
the framing call passes `indexdata=None`.

#### `--strip-tagdata`

Clears the `BlobHeader.tagdata` field (field 4, the per-blob tag key
index) on every OsmData blob, forcing `tags-filter`'s no-hint fallback
path. Exact structural mirror of `--strip-indexdata` for a different
header field: `indexdata` is preserved unless `--strip-indexdata` is
also set, so a tagdata-stripped file is still indexed (and its index
keeps its exact original bytes - a v1 index is not upgraded to v2). On
the passthrough path the blob payload is not decompressed and only the
targeted field is dropped, every other header field (indexdata,
`WayMembers-v1`, unknown fields) passing through byte-for-byte; on the
decode path the framing call passes `tagdata=None`
in both `frame_owned` (worker full blocks) and `frame_and_write_batch`
(merge-thread tail). Composes with every other flag, and alone is
passthrough-eligible (does not force a decode).

#### `--strip-bbox`

Clears the `HeaderBlock.bbox` field (field 1) so the output declares no
file-level bounding box. Entirely an OSMHeader change - it never
touches an OsmData `BlobHeader` or blob payload, so it composes for
free with every element-level transformation. On the passthrough path
(no override) the bbox is removed via `strip_header_block_fields`
while every other `HeaderBlock` field - `source`, a non-default
`writingprogram`, custom optional features, the osmosis replication
metadata, and unknown/extension fields - survives byte-for-byte; on
the decode path the bbox is simply omitted from the `HeaderBuilder`
rebuild the decode path already performs. Accepts non-indexed input
(no precondition): the passthrough branch never calls
`require_indexdata`.

**Motivation.** Not an `extract` fallback trigger: `extract --bbox`
derives its region purely from the CLI `--bbox` argument and prunes
blobs via the per-blob `indexdata` bboxes, so it never reads
`HeaderBlock.bbox` at all, and a `--strip-bbox` output is exactly as
extractable as the original
(`degrade_strip_bbox_extract_bbox_matches_original` pins this). The
honest rationale is that `HeaderBlock.bbox` *is* read operationally in
two other places, and both need adversarial "bbox absent" coverage:
`inspect`'s `extract_header_metadata` and the `inspect --get
header.bbox` fast path. Beyond `inspect`, `--strip-bbox` also exercises
downstream metadata propagation and interop with external tools that
declare or expect a file-level extent.

#### `--drop-ids N:SEED`

Deterministically removes exactly `N` elements from the output so that
surviving ways/relations that referenced them become dangling
references - the primary consumer is `check --refs` slow-path /
error-recovery benchmarking, where the dangling count needs to be a
well-defined, testable function of the input and `N:SEED`. `N` is an
exact absolute count, not a rate: `output_element_count ==
input_element_count - N` exactly, on input where every element has a
unique `(kind, id)` (every valid PBF and every degrade fixture; degrade
does not police the precondition, so duplicate `(kind, id)` pairs can
drop more than `N`). `N == 0` is rejected at the CLI; `N` greater than
the input's total element count is a hard error once the true count is
known (before any output file is opened, so a rejected run leaves no
partial output).

Selection is global across all three kinds (not per kind, so nodes -
vastly more numerous than ways/relations - dominate the dropped set,
maximizing dangling references) and is a pure function of `N`, `SEED`,
and the input's `(kind, id)` pairs: every element gets an ordering key
`(drop_hash(kind, id, SEED), kind, id)` from a fully-specified
splitmix64-style hash, and the `N` elements with the smallest keys are
dropped. Same input + same `N:SEED` always selects the byte-identical
set and produces a byte-identical output; a different `SEED` (same
`N`) selects a different set. The selection itself runs as a bounded-
memory pre-pass (one global size-`N` max-heap, fed by
`parallel_classify_phase` per-blob top-K results) before the decode
emit phases, so peak memory during selection is `O(min(N, total
elements))`, independent of thread count.

Because dropping elements changes per-blob element counts, `--drop-ids`
always forces the decode path (Section "Implementation paths" above),
even when composed only with otherwise-passthrough-eligible flags
(`--strip-indexdata`, `--strip-tagdata`). It composes with every other
transformation flag: survivors are emitted in original stream order,
so `Sort.Type_then_ID` is preserved exactly as without `--drop-ids`
(dropping a subsequence of a monotone sequence stays monotone), and the
`--unsort` / `--unsort-intra` swap state machines operate purely on
survivors (a kind whose survivor count falls to or below `hold_at` does
not swap, per the existing unsort machinery).

### What v1 preserves

On the passthrough path: every blob payload byte (including LOW
inline coords), all element-level properties, the `Sort.Type_then_ID`
header flag, and `LocationsOnWays` when the input declared them. Every
`BlobHeader` field is copied through verbatim except the one(s) the
active strip flags target - so the `indexdata` and `tagdata` hints are
each preserved unless their own strip flag is set (and when preserved,
their exact original bytes survive: a v1 index stays v1), and
`pbfhogg.WayMembers-v1` (field 5) plus any unknown/extension header
fields always pass through unchanged.

The same is now true one level up, at the file-level OSMHeader: with no
`--generator`/`--output-header` override, `passthrough_header_bytes`
forwards the input `HeaderBlock` payload verbatim (field-identical -
the outer Blob envelope is still re-compressed, so it is not
blob-byte-identical) via `strip_header_block_fields`, clearing only the
bbox (field 1) under `--strip-bbox`. This closed a pre-existing gap:
before this change, *any* passthrough-eligible run (including a plain
`--strip-indexdata` or `--strip-tagdata` with no bbox involvement at
all) rebuilt the output header through
`HeaderBuilder::from_header`, which silently drops `source`, custom
optional features, and unknown/extension fields, and resets a
non-default `writingprogram` to `pbfhogg` - none of which those flags
were ever supposed to touch. The verbatim `HeaderBlock` forward fixes
that silent loss for every passthrough-eligible flag combination, not
just `--strip-bbox`. The lossy `HeaderBuilder::from_header` rebuild
still runs, deliberately, when `--generator`/`--output-header` is
present - the user asked for a header rewrite, so the override wins
and the bbox (if `--strip-bbox` is set) is omitted from the rebuild.

On the decode path: element IDs, tags, refs, members, OsmMetadata,
DenseNode encoding. `Sort.Type_then_ID` is preserved unless `--unsort`
or `--unsort-intra` clears it. `LocationsOnWays` is dropped by
`BlockBuilder` as on every
other decode-path command in pbfhogg (`repack`, `sort`, `tags-filter`,
etc.); `--strip-locations` makes the loss explicit. `--drop-ids`
preserves every one of these properties on the surviving elements; it
only removes the `N` selected elements (and any reference to them
becomes dangling by construction, which is the point).

### Validated at scale

Passthrough (commit `48685ba`, plantasjen, 2026-04-28):

| Flag                | Mode    | Wall  | Peak anon | UUID       |
|---------------------|---------|-------|-----------|------------|
| `--strip-indexdata` | bench   | 79 s  | 15 MB     | `dc99fc70` |
| `--strip-indexdata` | hotpath | 90 s  | -         | `a7fa3897` |
| `--strip-indexdata` | alloc   | 92 s  | -         | `3470474f` |

The passthrough stays under 16 MB RSS on planet and is safe in every
measurement mode; the blob bytes are not decompressed.

Decode path post per-kind port (commit `69a8bbc`, plantasjen,
2026-04-30):

| Flag                | Dataset | Wall   | Peak anon   | UUID       |
|---------------------|---------|--------|-------------|------------|
| `--strip-locations` | europe  | 2m35s  | 404 MB      | `0fb0772d` |
| `--strip-locations` | planet  | 6m22s  | **1.19 GB** | `ae9d590d` |
| `--unsort`          | europe  | 20m47s | 1.51 GB     | `e5ab68c4` |

Planet `--strip-locations` lands at 1.19 GB peak anon and 6m22s
wall on plantasjen (28 GB RAM, 8 GB swap), down from the prior
~29 GB OOM at the same scale (commit `48685ba` overnight run).
Memory ceiling is in the same class as `repack` at planet
(1.36 GB / 380 s) - exactly what the per-kind port was designed
to deliver.

`--unsort` at europe runs at 1.51 GB peak (still bounded; only
one phase in flight at a time) but wall is 8x `--strip-locations`
at the same scale because all encoding serializes through the
merge thread. Planet `--unsort` not measured (linear projection
~2-3 hours); structurally OOM-safe by the same per-kind bound
that makes `--strip-locations` planet-safe. The wall cost is
the deferred-optimization motivation in
[v2.7](#v27---unsort-throughput-recovery).

### Tests

[`tests/cli_degrade.rs`](../tests/cli_degrade.rs):

- `degrade_strip_indexdata_drops_indexdata` - output has no
  `BlobHeader.indexdata` on the first OsmData blob; sortedness
  preserved; element multiset round-trips.
- `degrade_strip_locations_clears_low_and_preserves_elements` -
  output header has no `LocationsOnWays`; element data round-trips.
- `degrade_strip_tagdata_drops_tagdata` - *no* OsmData blob carries
  tagdata (`assert_no_tagdata_all_blobs` walks every blob, not just the
  first); indexdata, sortedness, and the element multiset are all
  preserved (passthrough path). Asserts a precondition
  (`count_tagdata_blobs(input) > 1`) that the fixture carried tagdata in
  more than one blob so the whole-file strip assertion is meaningful.
  The fixture tags nodes in two node blobs plus a way and a relation.
- `strip_blob_header_fields_preserves_untargeted_fields_verbatim`
  (unit test, `src/write/framing.rs`) - pins the passthrough
  field-preservation contract directly on the helper: stripping field 4
  (or field 2, or both) drops only the targeted field and copies every
  other field byte-for-byte, including a hand-built 26-byte v1 index
  (stays v1), a `WayMembers-v1` payload (field 5), and an unknown
  extension field. This is the regression pin for the two Medium review
  findings; a full end-to-end PBF `WayMembers` fixture would need the
  altw inject-prepass pipeline, so the invariant is pinned at the
  helper level where a `WayMembers` payload is just bytes.
- `strip_header_block_fields_preserves_untargeted_fields_verbatim`
  (unit test, `src/write/framing.rs`) - the `HeaderBlock` counterpart:
  stripping the bbox (field 1) from a hand-built `HeaderBlock` carrying
  a bbox, `source`, a non-default `writingprogram`, a custom optional
  feature, and the three osmosis replication fields drops only the
  bbox and copies every other field byte-for-byte, re-parsed back to
  confirm `source`, `writingprogram`, the custom feature, sortedness,
  and the replication metadata all survive.
- `degrade_strip_bbox_clears_header_bbox` - `--strip-bbox` on an
  indexed, rich-header bbox fixture: output has no `HeaderBlock.bbox`;
  `source`, `writingprogram`, the custom optional feature, replication
  metadata, and sortedness all survive verbatim
  (`assert_rich_header_fields_survived`); indexdata is untouched; every
  OsmData blob frame is byte-identical to the input
  (`osm_data_frames`); element multiset round-trips.
- `degrade_strip_bbox_no_indexdata_uses_passthrough` - `--strip-bbox`
  on a *non-indexed* bbox-bearing input succeeds without `--force`,
  proving the run stayed on the header-only passthrough (the decode
  path's `require_indexdata` precondition would otherwise fail it);
  output stays non-indexed, bbox is gone, other header fields survive,
  OsmData frames are byte-identical.
- `degrade_strip_bbox_and_strip_indexdata_compose` - bbox dropped from
  the header *and* indexdata cleared from every OsmData blob;
  sortedness and element multiset survive.
- `degrade_strip_bbox_and_strip_locations_compose` - composes across
  the path boundary: `--strip-locations` forces the decode path and
  `--strip-bbox` still clears the bbox from the rebuilt output header,
  confirming the bbox strip is wired into both paths.
- `degrade_strip_bbox_extract_bbox_matches_original` - end-to-end pin
  of the motivation section's "not an extract fallback" claim:
  `extract --bbox` on the stripped output and on the original
  bbox-bearing input select the identical element set (with a guard
  that the sub-region extract is both non-empty and strictly smaller
  than the whole file, so the comparison is not vacuous).
- `degrade_strip_bbox_with_generator_override_rebuilds_header` -
  `--strip-bbox --generator <name>` takes `passthrough_header_bytes`'
  rebuild branch instead of the verbatim branch: the bbox is still gone
  (the strip still applied) AND `writingprogram` equals the override
  rather than the fixture's original value (the override only takes
  effect on the rebuild path). Direct coverage for the rebuild branch,
  which none of the other `--strip-bbox` tests above exercise.
- `degrade_unsort_creates_adjacent_overlap_per_kind` - output header
  has no `Sort.Type_then_ID`; each kind has exactly one adjacent
  same-kind blob pair with overlapping ID ranges and zero intra-blob
  inversions; element multiset round-trips.
- `degrade_unsort_intra_creates_intra_blob_inversion` - output header
  has no `Sort.Type_then_ID`; each kind has exactly one intra-blob
  inversion and zero cross-blob overlaps; element multiset round-trips.
- `degrade_unsort_intra_large_input_blobs_stay_intra_blob` - the
  finding-1 regime: input packed at 20 elements/blob, cap 5, so one
  input blob spans four output blocks. `--unsort-intra` still yields
  the intra-blob shape (the old cap-boundary swap produced cross-blob
  overlap here).
- `degrade_unsort_and_unsort_intra_are_mutually_exclusive` -
  validation: the two unsort modes cannot be combined.
- `degrade_unsort_then_sort_round_trips` - the design's primary
  consumer loop: `degrade --unsort` then `pbfhogg sort`. Asserts sort's
  stderr reports blobs in overlap runs (the overlap-rewrite path fired,
  not passthrough), the resorted file is monotone in blob order
  (`assert_sorted_file`, which - unlike `read_normalized` - does not
  re-sort before checking), and the original element set is recovered.
- `degrade_unsort_and_strip_indexdata_compose` - composition test:
  output is unsorted *and* unindexed.
- `degrade_unsort_and_strip_locations_compose` /
  `degrade_unsort_intra_and_strip_locations_compose` - each unsort mode
  keeps its blob shape while `LocationsOnWays` is cleared.
- `degrade_unsort_intra_and_strip_indexdata_compose` - intra-blob
  unsorted *and* unindexed.
- `degrade_strip_tagdata_and_strip_indexdata_compose` - passthrough
  path clears both header hints; payload and sortedness survive.
- `degrade_unsort_and_strip_tagdata_compose` - decode path: exercises
  the `frame_and_write_batch` `tagdata=None` path (merge thread) while
  the stream is unsorted.
- `degrade_strip_locations_and_strip_tagdata_compose` - decode path
  with `--block-cap 10` against 20-element input blobs so workers
  pre-frame full blocks: exercises the `frame_owned` `tagdata=None`
  path while `LocationsOnWays` is cleared.
- `degrade_drop_ids_and_strip_locations_compose` (tier 2) -
  `--drop-ids 10:16 --strip-locations`: count is input - 10,
  `LocationsOnWays` cleared, output still sorted.
- `degrade_drop_ids_and_strip_indexdata_compose` (tier 2) -
  `--drop-ids 10:16 --strip-indexdata`: count is input - 10,
  `assert_non_indexed(&output)`.
- `degrade_drop_ids_and_unsort_compose` (tier 2) - `--drop-ids 10:16
  --unsort --block-cap 10` on the unsort fixture (60 nodes / 24 ways /
  24 relations, so every kind stays well above `hold_at` after 10 are
  dropped): count is input - 10, header not sorted, and each kind still
  shows exactly one adjacent cross-blob overlap and zero intra-blob
  inversions - proving the drop filter and the unsort swap compose
  without disturbing each other's shape.
- `degrade_requires_at_least_one_flag` - validation: no flags is a
  hard error.
- `degrade_rejects_zero_block_cap` - validation: `--block-cap 0`
  is a hard error.
- `degrade_unsort_intra_rejects_block_cap_one` - validation:
  `--unsort-intra --block-cap 1` is a hard error (intra shape
  impossible at cap 1).
- `degrade_unsort_accepts_block_cap_one` - `--unsort --block-cap 1`
  is supported and still yields the one-overlap-per-kind shape.
- `degrade_drop_ids_removes_exactly_n` - `--drop-ids 10:1` removes
  exactly 10 elements (`read_normalized` count) and leaves the output
  sorted (`is_sorted()` and `assert_sorted_file`), confirming that
  dropping a subsequence of a monotone stream stays monotone.
- `degrade_drop_ids_dangling_refs_match_check_refs` - the consumer
  contract: `--drop-ids 10:16` (pinned to drop nodes 2 and 3, referenced
  by every way, and way 2, a member of every relation) produces a
  `check --refs --check-relations --json` report whose four
  `missing_*` fields match expectations computed hash-independently
  from the output's surviving refs/members vs. surviving ids, and the
  four-field sum is asserted `> 0` so the run cannot pass vacuously.
- `degrade_drop_ids_is_reproducible_and_seed_changes_selection` - two
  runs of `--drop-ids 10:7` produce byte-identical output files;
  `--drop-ids 10:8` on the same input produces a different file.
- `degrade_drop_ids_validates_arguments_and_total` - table-driven:
  `0:1` rejects with "N must be >= 1", bare `10` (no seed) rejects with
  "N:SEED", and `1000000:1` against the small fixture rejects with
  "input has only" (N greater than the input's total element count).

Each unsort-mode composition test reuses the same shape helpers
(`assert_unsort_cross_blob_shape` / `assert_unsort_intra_shape`), so a
regression in the swap logic surfaces under every flag pairing, not
just the standalone case.

The `--drop-ids` hash and selection primitives are private, so they are
pinned by inline unit tests in `src/commands/degrade/mod.rs`
(`#[cfg(test)] mod tests`) rather than in `tests/cli_degrade.rs`:

- `drop_hash_golden_vectors` - pins the splitmix64 finalizer and
  `drop_hash` against fixed constants, including seeds that isolate a
  low bit and bit 32 (the guard against a 64-bit-seed collapse).
- `drop_selection_matches_full_sort` - the size-N max-heap top-K helper
  matches a full sort-and-truncate, for `N < len`, `N == len`, and
  `N > len`.
- `drop_selection_permutation_invariant` - the same key multiset in
  different orders selects the identical N-smallest set.
- `drop_selection_partition_invariant` - splitting keys into chunks
  (simulating per-blob worker results), reducing each chunk locally,
  and merging through one global heap matches the single-pass top-K -
  pinning the worker/merge decomposition as order- and
  partition-independent.
- `drop_key_orders_by_hash_then_kind_then_id` - `DropKey`s with an
  identical hash but differing `(kind, id)` order deterministically by
  `kind` then `id`, regardless of insertion order.

## Fixed

### `--unsort` produced intra-blob disorder instead of cross-blob overlap (found 2026-07-10, fixed 2026-07-11)

The merge loop's sort-preservation flush fired at every input-blob
boundary, so the central builder never spanned input blobs. On
Geofabrik input (~7,998 elements/blob, below the 8,000 cap) the
cap-keyed swap of global elements #8000/#8001 landed entirely inside
one output blob - an intra-blob inversion with non-overlapping blob
ranges. `detect_overlaps` returned 0 and `sort` passed the whole file
through, so its overlap-rewrite path had never actually run on real
unsorted data despite `unsort_fired=true` reporting success (run
`f5cd6522`: `sort_blobs_overlap=0`, full passthrough).

Fix (option (a) plus the deliberate second shape):

- `--unsort` now suppresses the boundary flush (`suppress_boundary_flush`),
  so the central builder packs continuously to `block_cap` and the
  cap-boundary swap straddles a genuine output-blob boundary
  (cross-blob overlap, as designed).
- The old intra-blob shape is preserved as the new `--unsort-intra`
  flag, which keys its swap to the first two same-kind elements rather
  than the cap boundary. That placement is robustly intra-blob for any
  `block_cap >= 2`, independent of input blob size - fixing the further
  bug that a large input blob (more than `block_cap` same-kind
  elements) would have made the shared cap-boundary swap produce
  cross-blob overlap instead.

Remaining follow-up (pending, run by the orchestrator): regenerate the
`unsorted` snapshot with `degrade --unsort --as-snapshot unsorted
--replace-snapshot`, then `verify sort --snapshot unsorted` as the
overlap-rewrite correctness gate.

## v2 scope

The deferred transformations from the design doc, ordered by likely
demand. Pick them up as benchmarking work surfaces consumers.

(v2.1 `--strip-tagdata`, v2.2 `--strip-bbox`, and v2.4 `--drop-ids`
shipped - see the transformations and test inventory above.)

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

### v2.7 - `--unsort` throughput recovery

**Goal:** close the Denmark `--unsort` regression introduced by the
per-kind classify port (denmark bench 7.7 s -> 27.3 s, 3.5x slower at
commit `13eed79`). Planet completes where it previously OOM'd, but
the per-element cost grew because workers now serialize each
matching element into `OwnedNode/OwnedWay/OwnedRelation` (Vec of
String pairs for tags, Vec for refs/members) and the merge thread
deserializes them back into the central `BlockBuilder`. The old
single-pass design went `Element<'_> -> BlockBuilder` directly,
amortizing one allocation per element instead of two.

**Why deferred:** `--unsort` is a one-time generation step (run
once, cache the result) feeding `sort`'s overlap-rewrite path. Even
at planet the projected wall (~30-40 min linear from per-element
overhead) is fine for that workload. Land when a consumer surfaces
that runs `--unsort` interactively or in a tight benchmarking loop.

**Three sketches, ordered by complexity:**

1. **Eliminate the Owned roundtrip.** Workers ship the full decoded
   `PrimitiveBlock` (or its decompressed bytes) to the merge thread
   instead of `Owned*`. Merge iterates `block.elements()` borrowed
   straight into the BlockBuilder. Per-slot memory grows from ~50 KB
   to ~80 MB at planet (the decoded block is the cost), 32 slots
   = ~2.5 GB - bounded, still well under repack's 1.36 GB ceiling
   only because the per-kind sequencing keeps just one phase in
   flight, but it does push the working set higher than current.
   Requires extending `parallel_classify_phase` to let workers ship
   an owning `PrimitiveBlock` (today the closure receives `&PrimitiveBlock`
   and the block is dropped after the closure returns).

2. **Restructure the swap to fire in the worker.** The swap is one
   event per kind, at the cap-1 boundary of the *first* input blob.
   If the worker handling input blob seq=0 of each kind does the
   swap inline (it can: it knows it's the first blob via seq=0, it
   knows the kind it's filtering for, it knows the cap), then
   subsequent blobs follow the `--strip-locations` shape and get
   parallel re-encoding. The worker for blob 0 still runs serially
   but only for ~`cap+1` elements. Requires extending
   `parallel_classify_phase` to pass the schedule entry's seq to
   the closure (today the closure signature is
   `Fn(&PrimitiveBlock, &mut S) -> R` with no seq). Edge case: blob
   0 having fewer than `cap+1` matching elements - swap doesn't
   fire there, has to roll into blob 1 (and possibly later) until
   enough elements arrive. State across workers is tricky; might
   degenerate to "ship Owned\* until swap done, then pre-frame"
   coordinated via the merge thread setting an atomic flag workers
   read.

3. **Two-pass approach.** Pass 1: re-encode without swap (basically
   `repack` with cap unchanged). Pass 2: byte-splice the first ~2
   blocks per kind to apply the swap. Pass 1 gets full parallel
   re-encode; pass 2 touches a tiny fraction of the file. Requires
   blob-level rewrite plumbing that doesn't exist today; the
   complexity is in pass 2's careful blob boundary handling.

Sketch 2 is the cleanest if `parallel_classify_phase`'s closure
signature is something we'd want to widen anyway (other callers
might also benefit from seeing the seq). Sketch 1 is the smallest
delta but pushes peak memory up. Sketch 3 is structurally separate
from the existing pipeline and earns its weight only if pass 2 has
other consumers.

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
  `frame_blob_pipelined` and `encode_blob_header_into` (decode path),
  and `strip_blob_header_fields` / `strip_header_block_fields`, the
  surgical wire-level field-omission helpers (sharing one
  `strip_message_fields` core) the passthrough path uses to preserve
  every untargeted `BlobHeader` field and `HeaderBlock` field
  byte-for-byte, respectively. The decode-path flush goes through
  the framing helpers directly when `--strip-indexdata` composes,
  bypassing the indexdata-embedding `write_primitive_block_owned` path.
