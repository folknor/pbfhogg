# Raw passthrough for extract

## Status: Phase 1 shipped, beats osmium

Simple extract now beats osmium (4.4s vs 7.2s Japan, 100s Europe) via a
different approach than originally planned: parallel blob classification
+ raw frame passthrough, rather than per-group raw copy within blocks.

## What shipped

### Parallel blob classification (the big win)

The 3-phase barrier pipeline classifies blobs in parallel using lightweight
scanners (node-only scanner for bbox test, way-ref scanner for ref check).
Workers pread + decompress + scan, send matching IDs to consumer.

- Node classify: 124s → 13s (-90%)
- Way classify: 81s → 6s (-93%)

### Raw frame passthrough (for contained node blobs)

Node blobs fully contained in the extract bbox (`BlobBbox::contains`) are
written as raw compressed frames — zero decompression, zero re-compression.
Consumer preads the raw frame directly and calls `write_raw_owned`.

Gated on: `Region::Bbox` only, `clean` is no-op, v2 bbox indexdata present.

### Infrastructure

Four primitives for future per-group passthrough (committed but not yet
used by the blob-level approach):
- `PrimitiveBlock::raw_group_bytes(index)` — raw PrimitiveGroup bytes
- `PrimitiveBlock::raw_stringtable_bytes()` — raw StringTable bytes
- `PrimitiveBlock::block_scalars()` — granularity, lat/lon offset
- `frame_raw_block()` — assemble PrimitiveBlock from raw components

## What remains

### For extract

- **Relation classify parallelization**: 13s at Europe (13% of simple total).
  Marginal return.
- **Complete/smart pass 1**: now uses three-phase parallel pread classification
  via `parallel_classify_phase` (Japan complete 19.7s → 4.8s, smart 24.3s → 9.0s).
  All three strategies now beat osmium.
- **Complete/smart write paths (pass 2+)**: still use pread-from-workers with full
  decode + re-encode. Raw group passthrough would help for groups where
  all elements are selected, but complete/smart do element-level filtering
  (partial matches common), making blob-level passthrough less applicable.

### For other commands

The four per-group primitives could be used by: cat --type, tags-filter,
getid, renumber, time-filter. These still fully decode + re-encode via
BlockBuilder. The approach: classify each group as all-match/partial/none,
copy all-match groups raw, re-encode partial groups.

Lower priority now that the highest-impact command (extract simple) already
beats osmium.

## Why the original per-group approach wasn't needed for extract

The original spec proposed copying raw PrimitiveGroup bytes for all-match
groups within each block. This requires:
- Per-element ID scanning to classify groups
- String table handling (copy whole or subset)
- No mixing raw and re-encoded groups (string table index alignment)

The parallel blob classification approach is simpler and faster because:
1. Classification happens at blob level (indexdata bbox for nodes, way-ref
   scanner for ways) — no element decode needed
2. Matching blobs are written as complete raw frames — no re-framing
3. The parallel pread workers overlap I/O with classification CPU
4. The consumer merges two streams (worker OwnedBlocks + raw frames) via
   the reorder buffer

The per-group approach would help for **partial-match blobs** at extract
boundaries, but the blob-level approach handles the interior (90%+ of blobs)
and the boundary blobs are few enough that full decode is acceptable.

## Reviewer sign-off

4/4 reviewers approved the blob-level approach as v1.
Parallel classification was identified by perf-Claude and perf-Codex as
the right "phase-barrier pipeline" architecture.
