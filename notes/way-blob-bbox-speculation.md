# Way blob spatial bboxes: impact speculation

> **STATUS 2026-07-13: speculative research, no work planned.**
> Chronological way-ID assignment caps the realistic skip rate at
> ~25-35 % for a Denmark-scale extract - far below what would justify
> format work - and the transformative variant (geography-sorted way
> blobs) breaks Sort.Type_then_ID. Linked from TODO.md's research /
> stretch list.

## The geographic clustering problem

In a Sort.Type_then_ID PBF, ways are sorted by OSM way ID. Way IDs
are assigned **chronologically** (sequential at creation time), not
geographically. A blob containing ways 100,000,000-100,008,000 has
ways created within a few-hour window by mappers worldwide.

**Consequence:** way blob bboxes would typically span large areas -
potentially the entire mapped world for recent-ID blobs (active
mapping happens globally). Only old-ID blobs (early OSM history, pre-
2010, predominantly European mapping) would have tight geographic
bboxes.

### Estimated way blob bbox sizes

- **Early IDs** (1-50M, ~2006-2010): mostly European mapping.
  Blob bboxes ~20-40° × 20-40°. Some geographic clustering.
- **Mid IDs** (50M-500M, ~2010-2018): global mapping ramps up.
  Blob bboxes ~60-120° × 40-80°. Weak clustering.
- **Recent IDs** (500M-1.17B, ~2018-2026): active global community.
  Blob bboxes likely span most of the mapped world. ~180° × 90°+.
  Essentially no spatial selectivity.

At 8000 ways/blob with 146K blobs, roughly:
- ~30K blobs (early IDs) might have useful bboxes (<50° span)
- ~50K blobs (mid IDs) have moderate bboxes
- ~66K blobs (recent IDs) have near-global bboxes

### Skip rate estimate for extract

**Denmark extract** (5° × 3.5° bbox):
- Early blobs: ~80% skip (small bboxes, mostly non-Danish)
- Mid blobs: ~30-50% skip (larger bboxes, some overlap)
- Recent blobs: ~5-10% skip (near-global bboxes)
- **Weighted average: ~25-35% skip** (not 50-80% as initially claimed)

**Europe extract** (70° × 38° bbox):
- Early blobs: ~30% skip
- Mid/recent blobs: ~5-15% skip
- **Weighted average: ~10-20% skip**

**Small city extract** (0.3° × 0.2° bbox, e.g. Copenhagen):
- Early blobs: ~95% skip
- Mid blobs: ~60-80% skip
- Recent blobs: ~10-20% skip
- **Weighted average: ~40-55% skip**

The skip rate is inversely proportional to the query region size and
proportional to how old the way IDs are. Not as dramatic as node blob
skip (where geographic clustering in ID space gives 95%+ skip for
small regions).

## Impact by pipeline step (speculative)

### extract simple - MODERATE win

Current planet extract (Europe bbox): ~200s total.
- Node classification: ~13s (parallel pread, blob-level spatial skip)
- Way classification: ~80s (parallel pread, NO spatial skip - must
  decode all way blobs to check refs against bbox_node_ids)
- Relation classification: ~10s
- Write phase: ~100s

With way blob bboxes, ~15% of way blobs skip decompression for Europe.
Way classification: 80s → ~68s. **Saving: ~12s (6% of total).**

For Copenhagen (small bbox): ~40% skip → 80s → ~48s. **Saving: ~32s.**

Not transformative but meaningful for small extracts from planet.

### extract complete/smart - MODERATE win

Same initial classification benefit. The dependency expansion passes
still process matching ways, so the benefit is bounded by the initial
spatial filter.

### multi-extract - GOOD win

For 10 regions covering ~30% of world area, the union bbox might
cover 60% of the world. ~40% of way blobs could skip for ALL regions.
For blobs that DO overlap the union, per-region bbox checks provide
additional selectivity.

At 100 regions covering ~50% of the world, ~50% of way blobs skip.
Combined with per-region selectivity, effective skip rate per region
could be 70-80%.

The multi-extract benefit scales with the number of regions because
the per-region checking is amortized across the single file read.

### build-geocode-index - NO win

Geocode builder processes the entire PBF. No spatial filtering.

### add-locations-to-ways - NO win

ALTW processes all ways. No spatial filtering.

### apply-changes (merge) - NO win

Merge is ID-based, not spatial.

### tags-filter - SMALL win (potential)

If tags-filter added spatial bbox filtering (currently not supported),
way blob bboxes would enable spatial skip. But tags-filter is tag-based,
not spatial. A `--bbox` flag for tags-filter would unlock this.

### diff/derive_changes - NO win

Diff is ID-based merge-join, not spatial.

### inspect --show - SMALL win (single element lookup)

Could skip way blobs whose spatial bbox doesn't contain the target
location. But `--show` uses ID-based lookup (via indexdata min/max ID),
which is already O(N) with good skip rates. Spatial info adds marginal
value.

## Alternative: geography-sorted way blobs

Instead of accepting chronological way IDs, **re-sort ways by
geographic centroid** during ALTW or cat. This would create way blobs
with tight geographic bboxes, dramatically improving spatial skip rates.

### How it would work

During ALTW (when way coordinates are available):
1. Compute each way's centroid from its node locations
2. Assign a spatial sort key (e.g., Hilbert curve value of centroid)
3. Sort ways by spatial key instead of way ID
4. Write to output PBF with spatial ordering

### Trade-offs

**Pros:**
- Way blob bboxes become tight (~1-5° span)
- 90%+ skip rate for small extracts from planet
- Better cache locality for spatial queries

**Cons:**
- Breaks Sort.Type_then_ID ordering (ways not sorted by ID)
- Other tools expect ID-sorted PBFs
- Binary search by way ID no longer works
- Must maintain a way_id→blob mapping for ID-based lookups

**Verdict:** too invasive for the standard pipeline. Could be an
alternative output mode (`--sort-spatial`) for use cases where spatial
queries dominate and ID lookup isn't needed.

### Hilbert curve specifics

The Hilbert curve maps 2D coordinates to a 1D value while preserving
spatial locality. Two nearby points on the curve are likely nearby in
2D. Using Hilbert values as sort keys groups geographically close
ways into the same blobs.

Libraries: `hilbert_2d` (Rust, 0 deps), `fast_hilbert` (Rust).
Computation: O(1) per point (bit interleaving).

At S2 cell level 15 (~1 km² cells), ~10M distinct cells cover the
mapped world. Ways within the same cell end up in the same blob group.

## Summary

| Scenario | Way blob skip rate | Time savings | Overall impact |
|----------|-------------------|-------------|----------------|
| Denmark extract from planet | ~30% | ~12s of ~200s | 6% |
| Copenhagen extract from planet | ~45% | ~32s of ~200s | 16% |
| Multi-extract 10 regions | ~40% per-region avg | ~32s of ~200s | 16% |
| Multi-extract 100 regions | ~70% per-region avg | ~56s of ~200s | 28% |
| With spatial sort | ~90%+ | ~72s of ~200s | 36% |

Way blob spatial bboxes help most for:
1. **Small extracts from planet** (city/country level)
2. **Multi-extract with many regions** (per-region selectivity compounds)
3. **Spatial-sorted PBFs** (dramatic improvement but breaks ID ordering)

They help least for:
1. **Large region extracts** (Europe from planet - low skip rate)
2. **Non-spatial commands** (merge, diff, tags-filter, ALTW, geocode)
3. **Already-extracted regional files** (Denmark PBF already has tight bboxes)
