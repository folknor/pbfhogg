# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    scripts/test.sh -- --ignored

## Performance: hotpath profiling results (Denmark 483 MB, Feb 2026)

**Primary consumers:** Both elivagar and nidhogg use the same pattern:
`ElementReader::from_path` -> `for_each_pipelined`. Nidhogg does 3 pipelined
passes per ingest. Nidhogg also uses `pbfhogg::merge::merge()` for weekly
planet updates. Both the read path and the merge write path are now heavily
optimized. The read path has decompression buffer pooling (`DecompressPool` +
`Bytes::from_owner`) and a zero-copy wire-format parser (`wire.rs`), yielding
**43-44% wall time improvement** across all single-threaded read modes. The
merge write path has parallel compression via `PbfWriter::to_path_pipelined`,
yielding **62% wall time improvement** (8.06s → 3.03s on Denmark). The top
remaining merge optimization is Finding 7 (skip decompression for passthrough
blobs).

Profiled with `hotpath-alloc` on two commands: `check-refs` and `tags-count`.
Run with `scripts/run-hotpath-alloc.sh <command> <file>`. Use `--min-count` for
tags-count to keep stdout manageable.

### Finding 1: `decode_blob` allocation churn — SOLVED

Originally 10.2 GB of cumulative alloc/dealloc for Denmark (21x amplification).
Fully solved in three steps:

- [x] **Reuse decompression buffers in merge.** `decompress_blob_data_into()` with
  caller-provided buffer. Merge passthrough blobs (91%) reuse the buffer.
- [x] **Investigate alternative allocators.** jemalloc/mimalloc showed <1% impact.
  Features kept for consumers who want lower RSS at planet scale.
- [x] **Pool decompression buffers in pipelined read path.** `DecompressPool` +
  `Bytes::from_owner()` returns Vec to pool on drop. Process alloc: 10.2 GB → 265 MB (-97%).
- [x] **Custom wire-format parser.** Eliminated remaining 9.3 GB of protobuf Vec
  allocations. See "Reduce protobuf parsing allocations" below.

### Finding 2: `tags_count` allocates 2.5 GB on the main thread (Denmark)

**Fixed.** Two-level `FxHashMap<String, FxHashMap<String, u64>>` with `get_mut(&str)`
lookups — no allocation for existing entries. Only new (key, value) pairs allocate.
Also removed `Box<dyn Iterator>` by inlining tag iteration per element type.

Results (Denmark):
- Wall time: 8.11s → **4.77s** (-41%)
- `main` alloc: 2.5 GB → **436 MB** (-83%)
- Main thread CPU: 100% → **11-16%** (no longer bottleneck)
- Process dealloc: 10.6 GB → **10.0 GB** (-600 MB, temporary Strings eliminated)

### Finding 3: main thread is the bottleneck in pipelined reads

**Partially fixed.** check-refs: main thread 100% CPU, workers 1% each (5 threads).
The decode workers are almost completely idle — RoaringTreemap insertions on the
main thread are the bottleneck. tags-count: main thread 100% CPU, pipelined
section takes 3.97s of 7.15s total (the remaining 3.18s is stdout I/O + hashmap
sorting). Both commands are consumer-bound, not worker-bound.

### Finding 4: `block::new` — wire-format parsing is cheap

After the wire parser rewrite, `PrimitiveBlock::new()` now does full wire-format
parsing (stringtable index + group range extraction). Allocates 18 KB avg
(stringtable `Vec<(u32,u32)>` + group_ranges), takes P50=5.6µs, P99=173µs,
total 107ms (1.5% of wall time). 130 MB cumulative for Denmark.

### Finding 5: missing hotpath instrumentation

Only `check_refs` is instrumented among the commands. Add `#[hotpath::measure]`
to the main function in each command file for per-command visibility:

- [ ] `tags_count.rs` — `tags_count()`
- [ ] `sort.rs` — `sort()`
- [ ] `cat.rs` — `cat()`
- [ ] `merge.rs` — `merge()`
- [ ] `extract.rs` — `extract()`
- [ ] `derive_changes.rs` — `derive_changes()`
- [ ] `diff.rs` — `diff()`
- [ ] `getid.rs` — `getid()`
- [ ] `tags_filter.rs` — `tags_filter()`
- [ ] `add_locations_to_ways.rs` — `add_locations_to_ways()`

### Raw data (current, with wire parser + pool)

**check-refs (Denmark, 52M nodes, 6.6M ways, 46K relations):**
```
Wall: 7.35s, RSS: 202 MB (with hotpath-alloc overhead)
decompress_blob:      7396 calls, 2.61s (35.6%), avg 354µs, 671 MB cumulative alloc (pooled)
block::new:           7396 calls, 113ms (1.5%), avg 15µs, 130 MB cumulative alloc
for_each_pipelined:   1 call, 7.34s (99.9%), 262 MB alloc
Process: 1.0 GB alloc, 1.3 GB dealloc.
Main thread: 100% CPU. Workers: 1% CPU each (5 threads, vastly idle).
```

**tags-count (Denmark, same file):**
```
Wall: 7.15s, RSS: 705 MB (with hotpath-alloc overhead)
for_each_pipelined:   1 call, 3.97s (55.6%), 436 MB alloc
decompress_blob:      7396 calls, 2.74s (38.3%), 692 MB cumulative alloc (pooled)
block::new:           7396 calls, 118ms (1.6%), 130 MB cumulative alloc
Process: 1016 MB alloc, 880 MB dealloc.
Main thread: 100% CPU.
```

**merge (Denmark base + 1 OSC diff, 630/7396 blobs rewritten):**
```
Wall: 3.15s, RSS: 91.9 MB (with hotpath-alloc overhead)
frame_blob:           628 calls, 5.67s total (parallel, 180% of wall), avg 9.0ms, 511 MB alloc
block_builder::take:  7407 calls, 735ms (23.3%), 266 MB alloc
block::new:           630 calls, 14ms (0.5%), 17 MB alloc
decode_blob:          1 call, 23µs (HeaderBlock only)
Process: 7.1 GB alloc, 7.0 GB dealloc. 4 worker threads + 1 writer thread.
Clean (no hotpath): 3.03s best of 3.
```

**bench-self (Denmark, best of 3, no hotpath overhead):**
```
sequential:  3076 ms
parallel:     302 ms
pipelined:   1599 ms
mmap:        3229 ms
blobreader:  3215 ms
```

### Finding 6: merge spends 66% of time compressing rewritten blobs — SOLVED

`frame_blob` (zlib compression) was 5.30s sequential for 632 blobs. Now runs
in parallel via `PbfWriter::to_path_pipelined`: rayon tasks compress blobs,
a dedicated writer thread reorders and writes them in sequence. Raw passthrough
blobs (`write_raw`) bypass rayon and go directly to the writer thread.

Result: **8.06s → 3.03s** (62% faster). 5.67s of compression spread across
4 worker threads. The writer thread runs at 5% CPU (I/O bound).

- [x] **Parallel compression in merge.** Implemented as `WritePipeline` in
  `writer.rs` with `to_path_pipelined` constructor.

### Finding 7: merge decompresses all blobs for ID range scanning

All 7,396 blobs are decompressed to scan ID ranges and decide passthrough vs
rewrite. With parallel compression solved (Finding 6), this decompression +
classification now dominates the remaining wall time. Buffer reuse via
`decompress_blob_data_into` eliminates allocation churn. The wire parser makes
actual block parsing for the 630 rewritten blobs very cheap (14ms, 17 MB alloc).

- [ ] **Skip decompression entirely for passthrough blobs in merge.** If blob
  headers or a pre-scan index can determine ID ranges without decompression,
  ~6,766 decompressions become unnecessary.

## Performance: parallelism

- [ ] `pipeline.rs:13-16` — `READ_AHEAD` / `DECODE_AHEAD` constants should be configurable
  or auto-tuned. Current `DECODE_AHEAD=32` means up to 64-256MB of decoded blocks in flight.

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  the landscape. Key options:
  - **paralight** (v0.0.8) — lightweight, targets slice/mut-slice parallelism. Can run on top of
    rayon's thread pool via `RayonThreadPool::new_global` (no extra threads). Has proper
    `try_for_each_init` that inits once per thread (rayon inits once per work item). Only needs
    `&` not `&mut` for the rayon backend. Limitation: no scopes, no graph algorithms, no recursive
    parallelism. Max `u32::MAX` elements.
  - **orx-parallel** — has `using()` API for guaranteed per-thread init. No thread pool yet
    (spawns threads per pipeline), on roadmap. No scopes/graph support.
  - **chili** — low-level, only provides `join`. A rayon fork (`par-iter`) builds par_iter on top
    of it. Uses lazy scheduling (less overhead for fine-grained work).
  - **forte** — experimental, rayon-like API with lazy scheduling. Supports spawn, join, scopes,
    scoped spawns. No par_iter or par_bridge yet.
  - **spindle** — built on rayon, optimised for small tasks. Very early.

  Wild's `thread_local` crate trick is also relevant: wrap per-thread state in
  `thread_local::ThreadLocal` and `.get_or()` inside rayon closures to guarantee one init per
  thread. Simple and works today without switching libraries.

  **Relevance to pbfhogg:** `par_map_reduce` uses rayon's `par_bridge` which has known overhead
  for ordered iteration. `for_each_pipelined` uses a custom 3-stage pipeline that doesn't depend
  on rayon's par_iter at all (it uses `rayon::spawn` for the decode pool). The main rayon
  consumer is merge's `par_bridge` in `classify_blob`. The `thread_local::ThreadLocal` trick
  could replace merge's `map_init(Vec::new, ...)` pattern for per-thread buffer reuse.

## Performance: parsing hot paths

- [x] **ID-only scan mode — not worth it.** check-refs is main-thread bound at
  100% CPU (RoaringTreemap). Decode workers run at 1% CPU. The wire parser already
  skips unnecessary fields during single-pass tag scanning.
- [x] **Selective parse for check_refs — not worth it.** Same conclusion: consumer-bound.
- [x] **Reduce protobuf parsing allocations (~9.3 GB for Denmark).** Implemented
  option (c): protozero-style custom wire-format decoder in `src/read/wire.rs`
  (~900 lines). All packed repeated fields (ids, lats, lons, refs, keys, vals)
  are now iterated on-the-fly from raw bytes via `PackedIter` — zero Vec alloc.
  `WireStringTable` stores `Vec<(u32,u32)>` offsets (8 bytes/entry vs 32).
  `PrimitiveBlock` owns `Bytes` + `WireBlock<'static>` (self-referential struct
  with lifetime erased via unsafe transmute). HeaderBlock and write path stay on
  `protobuf` crate. Results (Denmark):
  - Cumulative decode alloc: 9.3 GB → 130 MB (block::new only, **-98.6%**)
  - Sequential: 5378 → 3076 ms (**-43%**)
  - Parallel: 541 → 302 ms (**-44%**)
  - Pipelined: 1599 ms (**-27%**)
  - Mmap/blobreader: ~3200 ms (**-43%**)

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
- [x] `dense.rs` — Lazy `DenseNodeInfo` decoding — **solved by wire parser.** The custom
  wire-format parser already achieves this: `DenseNodeInfoIter` now uses packed varint
  iterators that decode on-the-fly from raw bytes, rather than pre-materialized `Vec<i64>`
  arrays. No separate lazy decoding pass needed.

## Dependencies

- [ ] CLI-only dependencies (`clap`, `quick-xml`, `serde_json`) are runtime deps of the library
  crate. Library-only users pay the compile cost. Consider a `cli` feature gate or separate
  `pbfhogg-cli` binary crate.
- [ ] `protobuf` crate: currently v3.7 (stepancheg/rust-protobuf, community, approaching EOL).
  Only used for HeaderBlock parsing, blob envelope (BlobHeader/Blob), and write path.
  The hot read path (PrimitiveBlock) now uses the custom wire-format parser in `wire.rs`.
  Migration to v4 or prost would be lower-impact now since the performance-critical path
  no longer depends on it.

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
