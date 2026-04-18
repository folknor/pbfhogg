# Raw passthrough: remaining opportunities

## Blob-level raw passthrough (shipped)

Blob-level raw passthrough is shipped for three commands:
- **extract simple** - 3-phase barrier pipeline classifies blobs and
  writes matching raw frames via pread workers. Beats osmium (4.4s vs
  7.2s Japan, 100s vs 350s Europe).
- **cat --type** - matching blobs written as raw compressed frames.
  Planet 207s → 43s (4.8x).
- **getid --invert** - blobs with no ID-range intersection pass through
  raw. Denmark 1.9s → 0.5s, Japan 8.6s → 1.3s.
- **getid include** - skips decompression of non-intersecting blobs
  (planet 71.5s → 32.5s, 2.2x). Not full raw passthrough but same
  principle.

## Tags-filter raw passthrough - DISPROVEN 2026-04-18

**Do not re-attempt.** The load-bearing comment lives in
[`src/commands/tags_filter.rs`](../src/commands/tags_filter.rs) at the
head of the pass-2 worker; this section is the full post-mortem.

### History

Raw passthrough for all-match blobs was attempted and removed once before:
the `count_in_range >= blob_count` heuristic was unsound (extraneous IDs
from other blobs inflate the count). After that, the agreed correct
approach was a lightweight per-blob wire-format ID scanner that confirms
every element in the blob is in the pass-1 matched set before passing
through raw. The design reads:

1. Pass 1 builds the matched ID set (`IdSetDense`)
2. For each blob in pass 2: scan wire-format element IDs (field 1 of
   each element message - cheap, skips string table / tags / refs)
3. If every ID is in the matched set -> blob passes through raw
4. Otherwise -> full decode + filter as now

This is lighter than full PrimitiveBlock decode (no string table
parsing, no tag parsing, no ref/member parsing - just IDs).

Constraints identified during April 2026 investigation:
- Only works for the two-pass path (default mode, not `-R`), because
  the single-pass path evaluates tag expressions on the fly with no
  pre-computed ID set to check against.
- The blob-level `TagIndex` cannot substitute for the scanner: it records
  which *keys* appear in the blob, not which elements have which keys.
  For `building=*` the tag index says `building` is present, but not
  whether every element has it.

### Shadow-counter measurement (2026-04-18, commit `a5c6854`)

Before building the scanner, we added a shadow counter in the pass-2
worker that did the full ID-set classification per blob (all_included /
all_direct gates) without actually passing any blob through raw. Run on
planet at `w/highway=primary` (UUID `8c786794`, plantasjen):

| Metric | Value |
|---|---|
| Pass-2 blobs total | 50,364 |
| **Blobs that would qualify under `all_included`** | **0** |
| **Blobs that would qualify under `all_direct`** | **0** |
| Way blobs | 17,529 |
| Way elements total | 1,165,589,744 |
| Way elements included | 3,983,027 (**0.34 %**) |
| Node blobs | 32,835 |
| Node elements total | 10,447,738,627 |
| Node elements included | 42,283,465 (**0.40 %**) |

Zero qualifying blobs out of 50,364. Not one.

### Why it will not work for any other tag filter either

The math is hostile in the general case, not just for
`w/highway=primary`. A blob holds ~8,000 elements. At any realistic
per-element match rate - 0.34 % here, or a hypothetical 10 % for a
broader key like `building=*` - the probability of all ~8,000 elements
matching is vanishingly small.

PBFs are sorted by ID, not by geography or tag, so matching elements
are scattered uniformly across every blob rather than clustered into a
few. Geographic clustering of tags does exist in the source data but
is destroyed by ID-sort. A filter that *did* match 100 % of elements
in 100 % of blobs would be a filter that matches every element in the
PBF, and that case is already served (faster, without an ID scan) by
blob-level type/tag-index filtering upstream of the pass-2 worker.

The stricter `all_direct` gate (required when `--remove-tags` is set,
since raw passthrough cannot strip tags off non-direct matches) is
tighter than `all_included` and also measured 0.

### Resolution

- Shadow counter removed in the same commit that removed it from this
  note. See the pass-2 worker comment block in
  [`src/commands/tags_filter.rs`](../src/commands/tags_filter.rs) - it
  is the load-bearing pin that prevents re-entry.
- No wire-format ID scanner. No per-group scanner on this command
  either (per-group would be even more expensive to evaluate per blob
  for even fewer qualifying cases).
- Pread workers remain for planet safety (no cross-thread
  PrimitiveBlock retention), not for passthrough.

## Per-group raw passthrough primitives (committed, unused)

Four primitives exist for per-group passthrough within partial-match blobs:
- `PrimitiveBlock::raw_group_bytes(index)` - raw PrimitiveGroup bytes
- `PrimitiveBlock::raw_stringtable_bytes()` - raw StringTable bytes
- `PrimitiveBlock::block_scalars()` - granularity, lat/lon offset
- `frame_raw_block()` in `src/write/raw_passthrough.rs`

All four are `#[allow(dead_code)]` scaffolding.

### Use case: extract boundary blobs

Extract simple's blob-level passthrough handles the interior (90%+ of
blobs at typical scales). Boundary blobs - where some groups are inside
the bbox and some outside - still go through full decode + re-encode.

Per-group passthrough would copy matching groups raw while only
re-encoding partial groups. For a boundary blob with 5 groups where 3
are fully inside: copy 3 raw + original string table, re-encode 2.

**Constraints on mixing raw and re-encoded groups:**
- Raw groups reference the original string table indices. Re-encoded
  groups use a new BlockBuilder string table. You can't mix them in one
  PrimitiveBlock unless the string table aligns.
- Approach: copy the whole original string table + raw groups, only
  re-encode partial groups using the same string table. This means the
  re-encoded groups must reference original string table indices, not
  BlockBuilder's own table - requires a different encode path.
- Alternative: emit raw-only groups and re-encoded groups as separate
  output blobs. Simpler but increases blob count. Acceptable since
  boundary blobs are ~5-10% of the file.

**Verdict:** Small win at the boundary. The 90%+ interior passthrough
already captures the major savings. Per-group passthrough would help
the remaining ~5-10% of blobs but requires either string-table-aligned
encoding or split output. Not worth the complexity unless boundary blob
processing becomes the bottleneck (e.g., many small regions with high
boundary-to-interior ratio in multi-extract).

### Other potential consumers

- **tags-filter**: partial-match blobs where some groups' elements all
  match and some don't. Requires per-group knowledge of whether all
  elements match, which isn't available from blob-level data.
- **renumber/time-filter**: every element is modified, so raw passthrough
  does not apply. The win for these commands is write-path throughput.

## Summary

| Opportunity | Status | Priority |
|---|---|---|
| Blob-level passthrough (extract, cat, getid) | Shipped | Done |
| Tags-filter blob-level (ID scanner) | **DISPROVEN 2026-04-18** (0/50,364 blobs qualify at planet) | Closed |
| Per-group passthrough (boundary blobs) | Scaffolding committed | Low |
| Renumber/time-filter | Not applicable (every element modified) | N/A |
