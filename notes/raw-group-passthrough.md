# Raw group passthrough for extract

## Problem

Simple extract is 2.5x slower than osmium at Japan scale (18s vs 7.2s).
The gap is on the write side: pbfhogg fully decodes every matching element
(tags, refs, metadata) into Rust types, then re-encodes via BlockBuilder
(string table construction, delta encoding, wire-format serialization).
osmium copies raw protobuf group bytes for matching elements, modifying
only the string table.

## Approach: phased rollout, blob-level first

### Phase 1: Blob-level raw passthrough for node blobs (v1)

For node blobs whose indexdata bbox is fully contained in the extract
region (`BlobBbox::contains`), write the raw decompressed blob bytes
directly — zero decode, zero re-encode.

The decision is per-blob, not per-group:
- **Fully contained**: `extract_bbox.contains(blob_bbox)` → raw passthrough
- **Partially overlapping**: existing decode + re-encode path
- **Outside**: skipped by spatial blob filter (existing)

This is the simplest implementation (O(1) bbox check per blob) and covers
the highest-volume case. In sorted PBFs, node blobs have tight geographic
bboxes (~0.01° spread). For regional extracts, ~90% of intersection node
blobs are interior (fully contained).

**String table**: copied whole. No subsetting — unused entries add ~1-5 KB
per block, negligible waste (<1% of output). Subsetting would require
scanning raw bytes for string table indices and rewriting them.

**No mixing of raw and re-encoded groups in the same block.** If any group
in a block needs partial re-encode, the entire block goes through the
existing path. This avoids string table index alignment issues between
raw groups (original indices) and BlockBuilder groups (fresh indices).
Node blocks in sorted PBFs have exactly one DenseNodes group, so
per-block = per-group for nodes.

**`--clean` guard**: raw passthrough disabled when any metadata cleaning
is active (raw bytes preserve original metadata).

**`Region::Bbox` only**: blob bbox containment is only valid for
rectangular extracts. Polygon extracts fall back to element-level
classification (a blob bbox inside the polygon's bbox is not necessarily
inside the polygon itself).

### Phase 2: Per-group passthrough for way/relation blobs (later)

After phase 1 ships and counters show the all-match ratio, decide
whether way/relation group passthrough is worth the complexity.

Way/relation groups are sorted by element ID, not geography. The
all-match ratio depends on the extract region size relative to the PBF.
For large regions (Europe from planet), most way groups are all-match.
For small regions (Tokyo from Japan), way groups are more likely partial.

Classification requires scanning element IDs in each group:
- Ways: read field 1 (varint ID) from each Way message, check `matched_way_ids`
- Relations: read field 1 from each Relation message, check `matched_relation_ids`
- Early exit: return `Partial` as soon as both a match and non-match are seen

Low-level ID scanners should be shared (`src/read/wire.rs` or similar).
The classification decision logic stays in `extract.rs`.

## Infrastructure (committed)

Four primitives already exist (commit `1ad821e`):
- `PrimitiveBlock::raw_group_bytes(index)` — raw PrimitiveGroup bytes
- `PrimitiveBlock::raw_stringtable_bytes()` — raw StringTable bytes
- `PrimitiveBlock::block_scalars()` — granularity, lat/lon offset
- `frame_raw_block()` — assemble PrimitiveBlock from raw components

`BlobBbox::contains()` exists for the containment check.

## Integration points

### 3-phase simple extract (sorted single-pass)

Phase 1 (node write): the pread_execute workers check the blob's
indexdata bbox. If fully contained in the extract bbox and `!clean.any()`
and `Region::Bbox`: skip PrimitiveBlock construction entirely, call
`frame_raw_block` with the raw proto bytes and write the result.

Workers need the blob's indexdata for the containment check. Currently
`pread_execute` doesn't pass indexdata to workers. Options:
- Include the `BlobDesc.kind` + bbox in the descriptor
- Workers parse the BlobHeader themselves (cheap, ~50 bytes)
- Pre-classify in the schedule builder and tag each descriptor as
  passthrough/decode

Pre-classify is cleanest: the schedule builder already has the BlobHeader
from `next_header_with_data_offset`. Extend `BlobDesc` with a
`passthrough: bool` field. Workers check it before deciding to decode.

### Complete/smart write passes

Same integration via `pread_write_pass`. The block function closure
checks the passthrough flag and either copies raw bytes or does the
full extract_block_pass2/pass3.

## Expected impact

Node blobs are ~60% of a PBF by volume. At Japan scale with ~90%
all-match node blobs, phase 1 saves decode + re-encode for ~54% of
total output bytes. Estimated: 18s → ~10-12s for simple extract.

At Europe scale (full-continent bbox): nearly all node blobs are
interior. Phase 1 saves even more proportionally.

## Reviewer sign-off

4/4 reviewers (perf-Claude, perf-Codex, planet-Claude, planet-Codex):
- Phase 1 blob-level passthrough for nodes is the right v1
- Copy full string table, no subsetting
- Don't mix raw and re-encoded groups in same block
- Add counters for passthrough/decode per blob type before phase 2
- Way/relation group passthrough deferred until counters justify it
