# Raw passthrough: remaining opportunities

## Tags-filter raw passthrough

Raw passthrough for all-match blobs was attempted but removed:
`count_in_range >= blob_count` is unsound (extraneous IDs from other
blobs inflate the count). The correct approach: a lightweight per-blob
wire-format ID scanner that confirms every element in the blob is in the
matched set before passing through raw. See TODO.md.

## Per-group raw passthrough primitives (committed, unused)

Four primitives exist for per-group passthrough within partial-match blobs:
- `PrimitiveBlock::raw_group_bytes(index)` — raw PrimitiveGroup bytes
- `PrimitiveBlock::raw_stringtable_bytes()` — raw StringTable bytes
- `PrimitiveBlock::block_scalars()` — granularity, lat/lon offset
- `frame_raw_block()` in `src/write/raw_passthrough.rs`

Use case: extract complete/smart pass 2+ still decode+re-encode via
BlockBuilder. For blobs where some groups fully match and some don't,
all-match groups could be copied raw while partial groups re-encode.
Blob-level passthrough handles the interior (90%+ of blobs); this
targets the boundary blobs.

Constraints on mixing raw and re-encoded groups in one block:
- String table indices must align (raw groups reference the original table)
- Cannot mix raw and re-encoded groups if re-encoded groups use different
  string table indices
- Approach: copy whole string table + raw groups, only re-encode partial groups
