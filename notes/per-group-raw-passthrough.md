# Per-group raw passthrough: the design question and the measurements that shelved it

Status: **not built, and not worth building without new evidence.** The
scaffolding that used to carry this analysis - `src/write/raw_passthrough.rs`
(`frame_raw_block`), the `PrimitiveBlock::raw_group_bytes` /
`raw_stringtable_bytes` / `block_scalars` / `group_count` accessors in
`src/read/block.rs`, and the `WireBlock::raw_stringtable` / `raw_proto`
accessors in `src/read/wire.rs` that only those wrappers called - was deleted on
2026-08-20. It had no consumer, sat under a module-wide `allow(dead_code)`, and
the analysis below is the only part of it that had value. Recovering the code
is `git show` on that commit; recovering the reasoning is this file.

## The idea

For groups where every element is selected, copy the raw protobuf
`PrimitiveGroup` bytes through instead of decoding and re-encoding via
`BlockBuilder`. The `StringTable` is copied whole. Only partial-match groups
need a full decode plus re-encode.

Blob-level passthrough in `extract` / `cat` / `getid` already handles the
90 %-or-more-interior cases, and blob level is the right granularity whenever it
fires. Per-*group* passthrough would be the finer-grained version.

## The real design question

**Mixing raw and re-encoded groups in one output blob.** Raw groups carry the
original string-table indices; `BlockBuilder` issues fresh ones. Two viable
shapes:

1. *String-table-aligned re-encode*: copy the original `StringTable` whole and
   re-encode partial groups against those existing indices instead of
   `BlockBuilder`'s own table. Requires a different encode path on the write
   side; preserves one output blob per input blob.
2. *Split output*: emit raw-only groups and re-encoded groups as separate
   output blobs. Simpler - both paths already exist in the writer - but doubles
   the boundary-blob count, which is acceptable only if boundary blobs are a
   small fraction of the output.

## Why it is shelved: two shadow-counter measurements

The prerequisite for either shape is the same question blob-level gates face:
*how many blobs would actually qualify under this gate, on a real workload?*
Two such measurements have already disproven blob-level gates in different
commands at planet scale:

- **tags-filter** (2026-04-18, UUID `8c786794`, `w/highway=primary` on planet):
  **0 of 50,364** pass-2 blobs qualified for "all-elements-included" blob
  passthrough. Load-bearing pin in `src/commands/tags_filter/mod.rs`, pass-2
  worker.
- **multi-extract** (2026-04-20, UUID `dad573cb`, planet 5-region
  `--config --simple`): **0 of 32,835** node blobs qualified for
  all-N-contained or partial-contained passthrough. Load-bearing pin in
  `src/commands/extract/multi.rs::try_extract_multi_single_pass`.

Both results are structural rather than workload-specific. PBF blobs are
ID-sorted and OSM IDs are chronological, not geographic and not tag-coherent,
so an 8,000-element blob scatters across the planet in both dimensions.
Per-element match rates stay low and per-blob "all elements qualify" rates are
effectively zero.

## What a future attempt owes

Shadow-count first, at the *group* granularity this time, and prove the
qualifying fraction is non-trivial on a real workload before writing any code.
The two measurements above are about blobs; a group is smaller, so the
qualifying fraction is not automatically zero - but nothing measured yet
suggests it is high enough to pay for the string-table problem above.
