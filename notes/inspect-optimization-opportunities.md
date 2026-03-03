# `pbfhogg inspect` optimization opportunities

## Implemented (commit `fc76dfb`, 2026-03-03, dm6)

### Index-only fast path (opportunities 1, 2, 4)

Added two-mode inspect: `try_index_only_scan` (header-only reads, no
decompression) with automatic fallback to `full_decode_scan` (original path).

**Mode selection:**
- `--locations` → FullDecode (needs per-way element data)
- Otherwise → IndexOnly if all blobs have indexdata, else FullDecode

**Implementation:**
- `read_blob_header_only` in `src/commands/mod.rs` — reads 4-byte len +
  BlobHeader, parses indexdata, returns `BlobHeaderInfo`. Caller skips blob data
  via `FileReader::skip()`.
- `try_index_only_scan` in `inspect.rs` — uses `BlobIndex` for element counts,
  block types, ordering segments, ID ranges (inter-blob monotonicity).
  OsmHeader blob still read fully (small, needed for metadata).
- `accumulate_from_index` helper processes one blob's index metadata.
- `HeaderMeta` struct bundles header metadata (eliminated 8-arg function).
- `BlockInfo.raw` is `Option<usize>` — `--blocks` omits Raw column in
  index-only mode.
- `--locations` percentile sort done in place (no clone).

**Output differences in index-only mode:**
- `tagged_node_count` not shown (0 in index-only, suffix omitted)
- `--blocks` table has no Raw column
- `--id-ranges` uses inter-blob monotonicity (reliable for real-world PBFs)

**Measured performance (Denmark 473 MB, 59M elements, 7397 blobs):**
- Index-only: **36ms** (header scan + skip)
- Full decode: **3.9s** (decompress + parse + element walk)
- Speedup: **109x**

### Not implemented

- **IndexPlusNodes mode**: decode only node blobs for tagged_node_count.
  Deferred — tagged count is a minor detail, and decoding node blobs (~60% of
  file) would reduce the speedup significantly.
- **`elements_skip_metadata()`**: not needed — index-only mode skips element
  iteration entirely.

## Remaining opportunities

- [ ] **Buffered `--blocks` output.** Per-line `println!` can dominate on large
  files. Use locked stdout writer or build into `String` chunks.
- [ ] **Parallel decode for heavy flag combinations.** `--locations` and
  `--id-ranges --locations` could benefit from pipelined/parallel decode with
  ordered reduction. Complexity: ordering segment and monotonic checks are
  order-sensitive.
