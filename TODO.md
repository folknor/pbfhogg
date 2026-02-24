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
- [ ] Implement lightweight "scan" mode for protobuf blocks — extract only IDs without full
  parse (skip string table, tags, info, coordinates, refs). Used by `IndexedReader`,
  `check_refs`, and anywhere only IDs are needed.

## Performance: compression

- [ ] Add zstd read/write support. PBF proto defines `zstd_data` (field 7) but code doesn't
  handle it. Zstd decompression is 3-5x faster than zlib at equivalent ratios. Add via `zstd`
  crate, consider making it the default for writing.
- [x] `blob.rs:556-561` — zlib decompression fallback changed to `bytes.len() * 4` in both
  `decompress_blob_data` and `decode_blob`.

## Performance: data structures

- [x] `extract.rs`, `tags_filter.rs` — `BTreeSet<i64>` replaced with sorted `Vec<i64>` +
  `binary_search()`. ~5x memory reduction (8 bytes/entry vs ~40). Lazy sorting via boolean
  flags exploits OSM PBF node→way→relation ordering.
- [ ] `check_refs.rs:49-51` — `HashSet<i64>` for all node/way/relation IDs. ~72GB for planet.
  Use roaring bitmap or sorted vec with binary search.
- [x] `block_builder.rs` — write-side `StringTable` switched from `HashMap` (SipHash) to
  `FxHashMap` (rustc-hash). Faster hashing for short OSM tag strings, safe because write-side
  has no untrusted input (no DoS concern).

## Performance: memory / planet-scale

- [ ] `commands/sort.rs` — reads entire file into memory. Unusable for 80GB. Implement external
  merge sort: split into sorted chunks, write temporary PBF segments, merge back.
- [ ] `commands/check_refs.rs` — stores all IDs in `HashSet<i64>`. See data structures item
  above. Needs two-pass or bitmap approach for planet scale.
- [ ] `dense.rs:10-20` — `DenseNode` is ~96 bytes due to always-decoded `DenseNodeInfo` (~48
  bytes). Make info decoding lazy — store raw iterator state, decode only when `.info()` called.
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
