# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    scripts/test.sh -- --ignored

## Correctness & safety

- [x] `block.rs:~97-99` — `HeaderBBox` doc comments fixed (top/bottom lat were swapped).
- [x] `elements.rs:439` — `MemberType::Unknown(i32)` and `MemberId::Unknown(i32, i64)` variants
  added. No more panic on unknown protobuf enum values. All callers updated with Unknown arms.
- [x] `elements.rs:592`, `dense.rs:358` — Documented: `RawTagIter` / `DenseRawTagIter` return
  raw indices (no stringtable lookup), so `Result` is not needed. The higher-level `TagIter` /
  `DenseTagIter` that do lookups would need `Result` but changing them is too disruptive (50+
  call sites).

## Performance: I/O & buffering

- [x] `blob.rs:274` — `BufReader` bumped to 256KB for sequential reads. Comment added to
  both `BlobReader::from_path` and `IndexedReader::from_path` (explaining why BufReader
  is intentionally not used there, referencing commit a38c258).
- [x] `writer.rs:49` — `BufWriter` bumped to 256KB in `PbfWriter::to_path`.
- [x] `mmap_blob.rs:182` — `MmapBlobReader` now stores raw `memmap2::Mmap` directly instead
  of `Bytes`. Iteration uses plain `usize` offset arithmetic, `Bytes::copy_from_slice()` only
  for protobuf parsing. Eliminates ~48K atomic ops per 500 MB file (3 `slice()` per blob).

## Performance: parallelism

- [x] `pipeline.rs:54` — decode pool now uses `available_parallelism() - 2`, min 1,
  fallback 4.
- [x] `reader.rs:138-139` — `par_bridge()` replaced with two-phase batch-collect: sequentially
  collect compressed blobs into `Vec<Blob>`, then `into_par_iter()` for lock-free parallel
  decode+map+reduce. Eliminates mutex contention at high parallelism.
- [ ] `pipeline.rs:13-16` — `READ_AHEAD` / `DECODE_AHEAD` constants should be configurable
  or auto-tuned. Current `DECODE_AHEAD=32` means up to 64-256MB of decoded blocks in flight.
- [x] `pipeline.rs:84` — reorder buffer replaced with `VecDeque` (pre-allocated, ring buffer).

## Performance: allocations

- [x] `blob.rs:486-538` — public decode helpers now have dual signatures: original `&[u8]`
  variants (backward-compatible) plus `_from_bytes(&Bytes)` variants for zero-copy callers.
  Callers with `Vec<u8>` can use `Bytes::from(vec)` (O(1) wrap) to avoid the copy.
- [x] `block_builder.rs:40-47` — `StringTable::add` rewritten with `entry()` API (one alloc
  per new string instead of two).
- [x] `block_builder.rs:162-192` — `BlockBuilder::new()` pre-allocates dense vectors
  (8000 capacity for ids/lats/lons, 16000 for keys_vals, 8000 for metadata fields).
  Note: `take()` leaves zero-capacity Vecs — documented as acceptable, future optimization.
- [x] `cat.rs`, `add_locations_to_ways.rs` — per-element `.collect()` allocations replaced with
  hoisted reusable `Vec` buffers using `clear()` + `extend()`. Eliminates ~150M alloc/dealloc
  pairs for Denmark-sized files.
- [x] `writer.rs:111` — zlib encoder pre-allocates with `Vec::with_capacity(data.len() / 2)`.

## Performance: parsing hot paths

- [x] `block.rs:416-425` — `str_from_stringtable` re-validates UTF-8 on every tag lookup.
  With 8000 elements * 2-3 tags = 32-48K validations per block, most redundant. Validate
  the entire stringtable once at `PrimitiveBlock::new()` time, then use `from_utf8_unchecked`.
- [x] `dense.rs:156-166` — `DenseNodeIter` key_vals scanning replaced with direct index-based
  while loop (no chunks iterator, no per-pair bounds check).
- [x] `elements.rs`, `dense.rs` — `WayNodeLocationsIter`, `Node`, `DenseNode`, `DenseNodeIter`
  now cache `granularity`, `lat_offset`, `lon_offset` as plain `i64` fields at construction
  time. Eliminates protobuf `Option` check + default-value fallback on every coordinate access.
- [ ] **ID-only scan mode — probably not worth it.** The original idea was a lightweight
  protobuf parser that extracts only element IDs, skipping stringtable, tags, coordinates,
  refs, and metadata. Investigation (Feb 2025) found:
  (1) The main supposed consumer, `check_refs`, actually needs way `refs()` and relation
  `members()` — not just IDs. So a pure ID scan doesn't help it.
  (2) The only true ID-only consumer is `IndexedReader::update_element_id_ranges()`, which
  runs once per session and is not a hot path.
  (3) Decompression (zlib/zstd) is ~60% of total read time and unavoidable — even a perfect
  scan that skips ALL protobuf parsing would only save ~35-40% of the remaining ~40%.
  (4) A custom wire-format parser for 5 message types is ~200-400 lines that must stay in
  sync with the proto schema — significant maintenance burden for two non-critical consumers.
  **Not yet benchmarked** — these estimates are based on code analysis, not profiling. If
  profiling shows protobuf parsing is a larger fraction than estimated, revisit.
- [ ] **Selective parse for check_refs** — skip stringtable, tags, coordinates, and metadata
  but keep IDs + way refs + relation member IDs/types. Unlike the ID-only scan above, this
  targets fields that check_refs actually needs. A planet check_refs must decompress+parse
  ~2.5M blocks; skipping stringtable (~20-40% of block), coordinates (~30% for dense nodes),
  tags, and metadata could meaningfully reduce the protobuf parsing cost. Implementation
  would require custom wire-format parsing (same maintenance concern as above) or a two-tier
  protobuf parse where certain fields are conditionally skipped. **Not yet benchmarked.**

## Performance: compression

- [x] Zstd read/write support added. Read side handles `zstd_data` (field 7) in both
  `decompress_blob_data` and `decode_blob`. Write side adds `Compression::Zstd(level)`.
  Default remains zlib for compatibility — most tools don't read zstd PBFs yet.
- [x] `blob.rs:556-561` — zlib decompression fallback changed to `bytes.len() * 4` in both
  `decompress_blob_data` and `decode_blob`.

## Performance: data structures

- [x] `extract.rs`, `tags_filter.rs` — `BTreeSet<i64>` replaced with sorted `Vec<i64>` +
  `binary_search()`. ~5x memory reduction (8 bytes/entry vs ~40). Lazy sorting via boolean
  flags exploits OSM PBF node→way→relation ordering.
- [x] `check_refs.rs` — `HashSet<i64>` replaced with `roaring::RoaringTreemap`. Planet-scale
  memory: ~2-3 GB instead of ~400 GB (100x reduction). `i64→u64` via `cast_unsigned()`.
- [x] `block_builder.rs` — write-side `StringTable` switched from `HashMap` (SipHash) to
  `FxHashMap` (rustc-hash). Faster hashing for short OSM tag strings, safe because write-side
  has no untrusted input (no DoS concern).

## Performance: memory / planet-scale

- [ ] `commands/sort.rs` — reads entire PBF into fully-decoded owned Rust structs (`OwnedNode`,
  `OwnedWay`, `OwnedRelation` with heap-allocated String tags, metadata, refs, members).
  Planet estimate: ~9B nodes × ~140 bytes + ~1B ways × ~312 bytes + ~17M relations × ~500 bytes
  = **~1,400 GB RAM**. Even without metadata, nodes alone require ~430 GB. Osmium has the same
  limitation (in-memory sort, recommends splitting first for large files).
  **Solution: external merge sort in 3 phases:**
  Phase 1 — Partitioned streaming split: read blob-by-blob, decode, classify by type, write
  into sharded temp PBF files by type+ID range (~256MB per shard). Memory: one decoded block
  at a time (~few MB) + K open file handles.
  Phase 2 — Sort each shard: each shard fits in memory (~1-2 GB decoded), sort with existing
  `sort_by_key`, write back. For mostly-sorted input (typical: planet downloads are pre-sorted),
  most shards may already be in order — verify with O(n) scan of block min/max IDs.
  Phase 3 — Sequential concatenation: shards are non-overlapping ID ranges, so writing is just
  sequential concatenation per type (nodes shards in order, then ways, then relations) through
  `BlockBuilder` + `PbfWriter`. Memory: one `BlockBuilder` buffer (~few hundred KB).
  **Optimization: block-level raw passthrough** — for blobs already in correct position with
  non-overlapping ID ranges vs neighbors, use `PbfWriter::write_raw()` to copy compressed bytes
  directly without decode/re-encode. Only decode+re-sort blobs with overlapping ranges.
  **Total memory: ~2 GB regardless of input size.**
  Note: the existing `PbfWriter`/`BlockBuilder` are fully streaming and already support this —
  no writer changes needed.
- [ ] `dense.rs` — Lazy `DenseNodeInfo` decoding — **probably not worth it.** Investigation
  (Feb 2025) found the premise was wrong: only 1 `DenseNode` is alive at any time (iterator
  yields them one at a time, no production code collects them), so peak memory is ~136 bytes
  total, not ~136 × 8000 per block. The DenseInfo packed arrays are already fully decoded by
  protobuf deserialization — making `DenseNodeInfo` lazy only avoids reading 4-5 cached values
  per node (~5-10 ns). 10 of 16 call sites need `.info()`. Overall speedup: ~0.5-1%.
  Any lazy approach requires breaking API change or dual iterators. See `DenseNode` doc comment
  in dense.rs for full rationale. **Not yet benchmarked.**
- [ ] `blob.rs:63-67` / `block.rs:104` — `Blob` and `PrimitiveBlock` derive `Clone`, making
  accidental clones extremely expensive (atomic refcount on every `Bytes` in stringtable).
  Consider removing `Clone` or using `Arc<PrimitiveBlock>` for shared access.

## Code quality

- [x] `error.rs` — removed deprecated `description()` and `cause()`, replaced with `source()`.
- [x] `commands/osc.rs` — `OscRelMember::member_type` changed from `String` to `MemberType` enum.
  Test unwraps replaced with `?` error propagation.
- [x] Test helper duplication — extracted shared helpers to `tests/common/mod.rs` (~700 lines of
  duplication removed across 7 test files).
- [x] `lib.rs:88-104` — documented: both re-export strategies are required (wildcards for external
  API, module re-exports for internal `crate::blob::` paths used in 15+ files).
- [x] Audited `#[allow(clippy::unwrap_used)]` sites. Fixed `indexed.rs:147` (proper error
  propagation in `create_index()`). Remaining sites are test modules where `unwrap()` is
  idiomatic — kept allows with explanatory comments.
- [x] Coordinate method duplication — `impl_coordinate_conversions!` macro extracts shared
  `lat()`/`lon()`/`decimicro_lat()`/`decimicro_lon()` across `Node`, `DenseNode`,
  `WayNodeLocation`.
- [x] `blob.rs` — `MAX_BLOB_HEADER_SIZE` and `MAX_BLOB_MESSAGE_SIZE` changed from `static`
  to `const`.

## Dependencies

- [x] `memmap2` bumped from 0.5 to 0.9 (soundness fixes).
- [x] `protobuf` / `protobuf-codegen` bumped from 3.1 to 3.7 (varint decoding and packed field
  performance improvements).
- [x] `quick-xml` bumped from 0.37 to 0.39.
- [ ] CLI-only dependencies (`clap`, `quick-xml`, `serde_json`) are runtime deps of the library
  crate. Library-only users pay the compile cost. Consider a `cli` feature gate or separate
  `pbfhogg-cli` binary crate.

## Testing gaps

- [x] `MmapBlobReader` / `Mmap` — `tests/read_paths.rs` (mmap_matches_blobreader,
  mmap_blob_types_and_offsets, mmap_seek)
- [x] `pipeline.rs` — `tests/read_paths.rs` (pipelined_matches_sequential)
- [x] `par_map_reduce` — `tests/read_paths.rs` (par_map_reduce_count,
  par_map_reduce_collect_ids)
- [x] Error/corrupt input — `tests/corrupt_input.rs` (10 tests: empty, truncated, oversized,
  garbage, iteration-stops-after-error, mmap variants)
- [x] `BlobReader::seek()` / `seek_raw()` — `tests/read_paths.rs` (blobreader_seek_to_start,
  blobreader_blob_from_offset, blobreader_seek_raw, blobreader_next_header_skip_blob)

## Before crates.io publish

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Verify edition 2024 is intentional — most published crates use 2021 for broader compatibility
- [ ] Add `tests/test.osm.pbf` to version control (generated by `cargo run --example gen_test_pbf`)
- [ ] Make writing program configurable in `build_header()` instead of hardcoded "pbfhogg"
- [ ] Add doc comments to `writer.rs` public API (PbfWriter, Compression)
- [ ] Add doc comments to `block_builder.rs` public API (BlockBuilder, Metadata, MemberData)
- [ ] Add crate-level documentation for write/merge workflows (lib.rs)
- [ ] Publish to crates.io

## GitHub

- [ ] Write GitHub repo description and tags (openstreetmap, pbf, protobuf, osm, rust)
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

## Code TODOs

- [ ] `src/indexed.rs:42` — use `relation_ids` field in `IdRanges` filtering.
  Leave as-is until a concrete consumer exists. No current or planned command benefits.
  Implementing `read_relations_and_deps` requires 3+ passes (relations→ways→nodes) and
  recursive member resolution. See git log for full analysis (commit a38c258).

## Merge correctness

Merge is fully validated: 11 unit tests + 4-tool cross-validation (commit a38c258).
pbfhogg matches osmosis and osmconvert exactly; osmium diverges on delete semantics
(version-based vs unconditional). See git log and `scripts/xval-merge.sh` for details.

## Benchmarking

- [ ] Track peak RSS during reads and merges at scale. Denmark for CI, planet for release validation.

## Planned commands

- [ ] `pbfhogg extract --smart` strategy — three passes, complete boundary/multipolygon
  relations (all member ways + their nodes included even if outside the extract region).

- [ ] `pbfhogg add-locations-to-ways` mmap index backends — currently in-memory only
  (HashMap), which limits to country-scale PBFs. Add mmap dense and mmap sparse backends
  for planet-scale processing.

## CLI cross-validation

Commands need cross-validation against osmium-tool (and where applicable osmosis/osmconvert)
on real PBF data, like the merge cross-validation in `scripts/xval-merge.sh`. Run each
command with pbfhogg and osmium on the same input, diff the outputs.

- [x] `merge` — cross-validated against osmium, osmosis, osmconvert (commit a38c258)
- [ ] `sort` — compare `pbfhogg sort` vs `osmium sort` output
- [ ] `cat` — compare `pbfhogg cat` vs `osmium cat` output (with and without type filters)
- [ ] `extract` — compare bbox and polygon extract vs `osmium extract`
- [ ] `derive-changes` — compare OSC output vs `osmium derive-changes`
- [ ] `diff` — compare output vs `osmium diff`
- [ ] `add-locations-to-ways` — compare vs `osmium add-locations-to-ways`
- [ ] `tags-filter` — compare vs `osmium tags-filter`
- [ ] `getid` / `removeid` — compare vs `osmium getid` / `osmium removeid`
- [ ] `check-refs` — compare vs `osmium check-refs`

## Nice to have

- [ ] Consider adding `serde` feature for element serialization
