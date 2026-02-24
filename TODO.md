# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    scripts/test.sh -- --ignored

## Correctness & safety

- [ ] `block.rs:~97-99` — `HeaderBBox` doc comments have swapped lat descriptions (top/bottom)
- [ ] `elements.rs:439` — `MemberType::from()` can panic on unknown protobuf enum values.
  Should return `Result` or a fallback variant instead of `#[allow(unwrap_used)]`.
- [ ] `elements.rs:592`, `dense.rs:358` — `RawTagIter` / `DenseRawTagIter` silently produce
  potentially invalid data when string table lookups fail. Should return `Result` (marked TODO
  in code).

## Performance: I/O & buffering

- [ ] `blob.rs:274` — `BufReader` uses default 8KB buffer. Bump to 256KB+ for sequential reads.
  PBF blobs are 16-32KB compressed, so 8KB = 2-4 syscalls per blob. For 80GB (~2.5M blobs)
  that is 5-10M unnecessary syscalls. Note: `IndexedReader` was already investigated (commit
  a38c258) — BufReader has no benefit there due to random seeks. This item is about the
  sequential `BlobReader` path only. When implementing, add code comments to both
  `BlobReader` (explaining the larger buffer choice) and `IndexedReader::from_path`
  (explaining why BufReader is intentionally not used, referencing commit a38c258).
- [ ] `writer.rs:49` — `BufWriter` also uses default 8KB. Bump to match.
- [ ] `mmap_blob.rs:182` — `MmapBlobReader::next()` creates a `Bytes::slice()` (atomic clone)
  on every iteration. Track offset as plain `usize` and index into raw `&[u8]`, only wrapping
  in `Bytes` for protobuf parsing.

## Performance: parallelism

- [ ] `pipeline.rs:54` — decode pool hardcoded to 4 threads. Use
  `std::thread::available_parallelism()` minus 2 (IO + main). On 8+ core machines this
  roughly doubles pipelined throughput.
- [ ] `reader.rs:138-139` — `par_bridge()` in `par_map_reduce` has significant mutex contention
  at high parallelism. Consider pipeline-based approach or batch-collect into Vec then
  `par_iter()`.
- [ ] `pipeline.rs:13-16` — `READ_AHEAD` / `DECODE_AHEAD` constants should be configurable
  or auto-tuned. Current `DECODE_AHEAD=32` means up to 64-256MB of decoded blocks in flight.
- [ ] `pipeline.rs:84` — reorder buffer uses `HashMap<usize, ...>` for consecutive sequence
  numbers. Replace with `VecDeque` or fixed-size ring buffer to eliminate hashing overhead.

## Performance: allocations

- [ ] `blob.rs:486-538` — public decode helpers (`parse_blob_header`, `decode_blob_to_primitiveblock`,
  etc.) call `Bytes::from(slice.to_vec())`, copying input bytes. Accept `Bytes` directly or
  use `Bytes::copy_from_slice()`. Callers in merge path often already have owned `Vec<u8>`.
- [ ] `block_builder.rs:40-47` — `StringTable::add` allocates the same string twice (once for
  `strings` Vec, once for `index` HashMap). Use `entry()` API to allocate once.
- [ ] `block_builder.rs:162-192` — `BlockBuilder::new()` creates all Vecs with zero capacity.
  Pre-allocate: 8000 for dense_ids/lats/lons, ~16000 for dense_keys_vals. Also, `take()`
  leaves zero-capacity Vecs — swap in pre-allocated Vecs instead.
- [ ] `cat.rs:159,184,211,232`, `add_locations_to_ways.rs:227,251,278-279` — temporary Vecs
  created per-element in hot loops (`tags.collect()`, `refs.collect()`). Hoist reusable buffers
  outside the loop, use `clear()` + `extend()`.
- [ ] `writer.rs:111` — zlib encoder wraps `Vec::new()`. Pre-allocate with
  `Vec::with_capacity(uncompressed.len() / 2)`.

## Performance: parsing hot paths

- [ ] `block.rs:416-425` — `str_from_stringtable` re-validates UTF-8 on every tag lookup.
  With 8000 elements * 2-3 tags = 32-48K validations per block, most redundant. Validate
  the entire stringtable once at `PrimitiveBlock::new()` time and store `Vec<&str>`.
- [ ] `dense.rs:156-166` — `DenseNodeIter` uses `chunks(2)` for key_vals scanning, introducing
  bounds checking per pair. Replace with index-based while loop scanning for `0` delimiter.
- [ ] `elements.rs:410-418` — `WayNodeLocationsIter` calls `lat_offset()`, `lon_offset()`,
  `granularity()` (protobuf accessors through `Option` check) on every iteration. Cache as
  fields at construction time. Same for `DenseNode::nano_lat/lon` and `Node::nano_lat/lon`.
- [ ] Implement lightweight "scan" mode for protobuf blocks — extract only IDs without full
  parse (skip string table, tags, info, coordinates, refs). Used by `IndexedReader`,
  `check_refs`, and anywhere only IDs are needed.

## Performance: compression

- [ ] Add zstd read/write support. PBF proto defines `zstd_data` (field 7) but code doesn't
  handle it. Zstd decompression is 3-5x faster than zlib at equivalent ratios. Add via `zstd`
  crate, consider making it the default for writing.
- [ ] `blob.rs:556-561` — zlib decompression fallback when `raw_size` is missing uses compressed
  size as capacity (3-10x too small). Use `bytes.len() * 4` as fallback.

## Performance: data structures

- [ ] `extract.rs:374-375,443-446`, `tags_filter.rs:400-403` — `BTreeSet<i64>` for matched IDs.
  ~40 bytes/entry overhead. Replace with sorted `Vec<i64>` + binary search (~8 bytes/entry)
  or roaring bitmaps for dense sets.
- [ ] `check_refs.rs:49-51` — `HashSet<i64>` for all node/way/relation IDs. ~72GB for planet.
  Use roaring bitmap or sorted vec with binary search.
- [ ] `block_builder.rs:26` — write-side `StringTable` uses `HashMap<String, u32>` with default
  SipHash. Use `rustc_hash::FxHashMap` for faster hashing (short strings, no DoS concern).

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

- [ ] `error.rs` — implements deprecated `std::error::Error::description()` and `cause()`.
  Replace with `Display` impl and `source()`.
- [ ] `commands/osc.rs` — `OscRelMember::member_type` is `String` instead of enum. Should use
  `MemberType` enum for type safety.
- [ ] Test helper duplication — `make_node()`, `make_way()`, `make_relation()`, `roundtrip()`
  etc. are copy-pasted across 7+ test files. Extract into a shared `tests/common/mod.rs`.
- [ ] `lib.rs:88-104` — redundant re-export strategy: both wildcard item-level (`pub use read::blob::*`)
  and named module-level (`pub use read::blob`). Creates two paths to every public item.
  Pick one approach.
- [ ] Audit `#[allow(clippy::unwrap_used)]` sites (5+ occurrences: `elements.rs:439`,
  `blob.rs:572`, `indexed.rs:147,417`, `osc.rs:503`, `extract.rs:755`, `tags_filter.rs:629`,
  `getid.rs:398`). Convert to proper error handling where possible.
- [ ] Coordinate method duplication across `Node`, `DenseNode`, `WayNodeLocation` — three
  identical `lat()`/`lon()`/`nano_lat()`/`nano_lon()` implementations. Extract a trait or
  shared helper.
- [ ] `blob.rs` — `MAX_BLOB_HEADER_SIZE` and `MAX_BLOB_MESSAGE_SIZE` are `static` instead of
  `const`. Use `const` for compile-time constants.

## Dependencies

- [ ] `memmap2` pinned at 0.5, current is 0.9+. Newer versions have soundness fixes.
- [ ] Generally check for updates to all dependencies.
- [ ] `protobuf` / `protobuf-codegen` at 3.1 (2022). Update to 3.7+ for varint decoding and
  packed field performance improvements.
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
