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

## Tags-filter raw passthrough

Raw passthrough for all-match blobs was attempted but removed:
`count_in_range >= blob_count` is unsound (extraneous IDs from other
blobs inflate the count). The correct approach: a lightweight per-blob
wire-format ID scanner that confirms every element in the blob is in the
matched set before passing through raw. See TODO.md.

**Investigation (April 2026):** This approach only works for the
two-pass path (with `--add-referenced-ways`) where pass 1 has already
collected matched IDs into an `IdSetDense`. The single-pass path
evaluates tag expressions on the fly - there's no pre-computed ID set
to check against.

Even for the two-pass path, blob-level raw passthrough is limited by
what the `TagIndex` stores. The tag index records which *keys* appear
in the blob, not which elements have which keys. You can't determine
from blob-level data alone that ALL elements match a tag expression.
For `building=*` (key-only match), the tag index says `building` is
present in the blob, but not whether every element has it - some
elements may have no `building` tag at all.

The per-blob wire-format ID scanner approach would:
1. After pass 1 builds the matched ID set (IdSetDense)
2. For each blob in pass 2: scan wire-format element IDs (field 1 of
   each element message - cheap, skips string table / tags / refs)
3. If every ID is in the matched set → blob passes through raw
4. Otherwise → full decode + filter as now

This is lighter than full PrimitiveBlock decode (no string table
parsing, no tag parsing, no ref/member parsing - just IDs). The win
depends on what fraction of blobs are 100% matched. For broad filters
like `building=*`, most way blobs contain a mix of matching and
non-matching elements, so few blobs would qualify for passthrough.
For type-specific filters on sorted PBFs (e.g., all-relation
expressions), blob-level type filtering already skips non-matching
blobs entirely.

**Verdict:** Low priority. The benefit is bounded by the fraction of
100%-matched blobs, which is small for typical tag filter expressions.
The blob-level type + tag key filtering already skips the easy wins.

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
| Tags-filter blob-level (ID scanner) | Designed, not implemented | Low |
| Per-group passthrough (boundary blobs) | Scaffolding committed | Low |
| Renumber/time-filter | Not applicable (every element modified) | N/A |
