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
yielding **62% wall time improvement** (8.06s → 3.03s on Denmark). Finding 7
(blob-level indexdata) embeds element type + ID range in BlobHeader so that
subsequent merges can classify passthrough blobs without decompression — saving
~8% on Denmark (3.07s → 2.81s) with larger gains expected at planet scale.
O_DIRECT writes (`--direct-io`) add no measurable overhead on Denmark (2821ms
BufWriter vs 2876ms O_DIRECT, within noise) — the pipeline is CPU-bound on zlib
compression at this scale. The real win is page cache hygiene at planet scale
(80GB writes not evicting useful host data). `copy_file_range` for passthrough
blobs saves ~3% on Denmark (2.81s → 2.73s) by eliminating userspace buffer copy
for index-hit blobs — larger gains expected at planet scale where passthrough
dominates.

All command entry points, merge internals (`classify_blob`, `read_raw_frame`,
`rewrite_block`), and pipeline (`run_pipeline`) are instrumented with
`#[hotpath::measure]`. Run with `scripts/run-hotpath.sh` (timing only) or
`scripts/run-hotpath-alloc.sh` (timing + allocation tracking).

### Raw hotpath data (current, with wire parser + pool + blob indexdata)

**check-refs (Denmark, 52M nodes, 6.6M ways, 46K relations):**
```
Wall: 7.51s, RSS: 143 MB (hotpath timing, no alloc tracking)
check_refs:           1 call, 7.51s (100%)
run_pipeline:         1 call, 7.50s (99.9%)
for_each_pipelined:   1 call, 7.50s (99.9%)
decompress_blob:      7396 calls, 2.55s (33.9%), avg 345µs, P50 198µs, P99 1.72ms
block::new:           7396 calls, 97ms (1.3%), avg 13µs, P50 5µs, P99 166µs
Main thread: 100% CPU. Workers: 1% CPU each (5 threads, vastly idle).
```

**merge without indexdata (Denmark base + 1 OSC, osmium input, prost):**
```
Wall: 2.78s, RSS: 97 MB (hotpath timing)
frame_blob:           630 calls, 5.61s total (202%, parallel), avg 8.9ms
classify_blob:        7385 calls, 3.23s (116%), avg 438µs, P50 263µs, P99 2.12ms
rewrite_block:        630 calls, 1.49s (54%), avg 2.36ms, P50 2.83ms, P99 7.25ms
block_builder::take:  7407 calls, 739ms (27%), avg 100µs, P50 10ns, P99 2.08ms
read_raw_frame:       7399 calls, 82ms (3%), avg 11µs
block::new:           630 calls, 14ms (0.5%), avg 21µs
Clean (no hotpath): 2.61s best of 3.
```

**merge with indexdata (Denmark base + 1 OSC, pbfhogg input):**
```
Wall: 2.86s, RSS: 85 MB (hotpath timing)
frame_blob:           549 calls, 5.40s total (189%, parallel), avg 9.8ms
rewrite_block:        550 calls, 1.71s (59.7%), avg 3.11ms
block_builder::take:  7408 calls, 673ms (23.5%), avg 91µs
classify_blob:        7382 calls, 603ms (21.1%), avg 82µs, P50 150ns, P99 1.99ms
read_raw_frame:       7400 calls, 90ms (3.2%), avg 12µs
block::new:           631 calls, 14ms (0.5%), avg 23µs
Clean (no hotpath): 2.81s best of 3.
```

**bench-self (Denmark, best of 3, no hotpath overhead):**
```
sequential:  3076 ms
parallel:     302 ms
pipelined:   1599 ms
mmap:        3229 ms
blobreader:  3215 ms
```

### Merge: remaining optimization theories

With indexdata, the merge bottleneck is `rewrite_block` (60%) + `block_builder::take`
(24%) — the actual decode/re-encode work on the ~550 rewritten blocks. These
process ~4.4M elements (most unaffected by the diff) at Denmark scale.

- [x] **Element-level raw passthrough in rewrite_block** — investigated, not
  feasible. String table index coupling (all types) and cross-element delta
  encoding (dense nodes) make per-element raw byte splicing impossible without
  re-serialization that costs the same as full reencode.

- [ ] **Pre-seed output StringTable from input block** — investigated: less
  clear-cut than expected. `StringTable::add()` already allocates via
  `entry(s.to_owned())` on every call (even for existing strings), so pre-seeding
  doesn't save allocations — it just moves them earlier. The natural access
  pattern already populates the table on the first few elements, and subsequent
  elements hit the occupied path. The real win would be **avoiding the `add()`
  call entirely** for unmodified elements by preserving input string table indices
  in the output — but that requires a fundamentally different BlockBuilder mode
  where raw string-table indices pass through without re-interning. Worth
  prototyping to measure whether the FxHashMap lookup overhead on ~8000 elements
  × ~4 tags is actually significant.

- [ ] **Raw packed bytes for non-string integer fields** — investigated: the
  delta encoding is compatible (both input wire format and BlockBuilder delta-
  encode refs/memids from 0 within each element), so raw byte passthrough is
  valid. However, prost's generated `Way`/`Relation` types use `Vec<i64>` for
  `refs`/`memids` — accepting raw packed bytes would require either bypassing
  prost serialization for these fields (manual protobuf encoding) or a custom
  message type. The complexity may not be justified: refs/memids are a fraction
  of the `rewrite_block` cost compared to tag string interning and metadata
  handling. Profile first to see if ref/memid decode+reencode is a significant
  slice of the 1.49s `rewrite_block` total.

- [x] **Tag/ref Vec allocation churn** — hoisted reusable buffers in `rewrite_block`.

- [x] **BlockBuilder Vec reuse across blocks** — `reset()` preserves capacity.

- [x] **Protobuf serialization in `take`** — re-benchmarked with prost: 739ms
  (slightly slower than old crate's 673ms). `take()` is 27% of wall time; further
  gains need buffer reuse instead of `encode_to_vec()`.

## Performance: parallelism

- [ ] `pipeline.rs:13-16` — `READ_AHEAD=16` / `DECODE_AHEAD=32` are hardcoded.
  Making them configurable would require a pipeline config struct on the public
  `for_each_pipelined` API. Current values work well at both Denmark and planet
  scale — hotpath shows the pipeline is balanced (I/O thread not stalling, rayon
  workers barely loaded, main thread is the bottleneck). Memory cost is 32 blocks
  × ~32KB-1MB = 32-256MB peak. Low priority — configure when someone reports a
  problem on a memory-constrained system.

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

## Performance: memory / planet-scale

- [x] `commands/sort.rs` — blob-level permutation sort. O(num_blobs) memory,
  passthrough for non-overlapping blobs. Denmark: 3.0s, 100% passthrough.

## Performance: Linux kernel features for planet-scale I/O

Research notes: `docs/linux-async-io.md`.

Target deployment: nidhogg weekly planet merge on Linux 6.18, planet PBF on erofs.
Nidhogg will use erofs (atomic swap of entire planet data at runtime), so
`Compression::None` PBFs on erofs is the baseline assumption for the optimized path.
The library also needs to work well for the broader OSM ecosystem (standard
zlib-compressed PBFs, any filesystem, any Linux 5.x+), so there are two tiers.

### Tier 1: Generic path (any Linux, zlib PBFs, any filesystem)

The generic path is CPU-bound on zlib compression/decompression. io_uring adds
negligible value here (~30ms syscall savings on 80GB). Focus on page cache hygiene
and kernel-space copy.

- [x] **O_DIRECT writes + reads.** Feature-gated `linux-direct-io`. `DirectWriter`/
  `DirectReader` with page-aligned buffers, wrapped in `FileWriter`/`FileReader` enums.
  All commands accept `--direct-io`.

- [x] **`copy_file_range` for blob passthrough.** Kernel-space copy in merge/cat/sort.
  ~3% improvement on Denmark (2.73s vs 2.81s), larger gains at planet scale.

- [ ] **Large folios for mmap reads.** On 6.14+, file-backed mmap gets transparent
  2MB huge pages automatically. An 80GB mmap'd PBF goes from ~20M TLB entries
  (4KB pages) to ~40K entries (2MB folios). Combined with `MADV_POPULATE_READ`
  (5.14+) to prefault pages ahead, the mmap read path gets substantially faster.
  `MmapBlobReader` could use `MADV_SEQUENTIAL` + `MADV_POPULATE_READ` in chunks
  (e.g. 256MB ahead) for predictable prefaulting without committing all 80GB at once.

  **Caveat: low priority.** The mmap path (`MmapBlobReader`) is not the production
  hot path — elivagar and nidhogg use `for_each_pipelined` (read) and `merge`
  (write). Mmap is already the slowest read mode at Denmark scale (3.2s vs 0.3s
  parallel, 1.6s pipelined). `MADV_POPULATE_READ` adds upfront page fault cost
  that would hurt country-scale files (~120K faults for 483MB) where TLB pressure
  isn't the bottleneck. The win is planet-scale only (80GB, 20M TLB entries). If
  implemented, should be opt-in (`MmapBlobReader::with_prefault(true)` or similar)
  to avoid regressing small-file performance. Consider skipping entirely in favor
  of Tier 2 work (io_uring + erofs) which targets the actual production path.

### Tier 2: erofs + io_uring (nidhogg, Linux 6.14+, Compression::None)

With erofs + `Compression::None`, zlib is eliminated entirely. erofs handles lz4 in
kernel at ~4 GB/s (SIMD-optimized), `decompress_blob` becomes a no-op, and the
pipeline becomes **I/O-bound**. Now io_uring's batched async writes and registered
buffers actually matter — the writer thread is the bottleneck, not compression.

- [x] **erofs + uncompressed PBFs.** `--compression` flag on all write commands,
  `Compression` enum public API. On erofs: single lz4 decompression layer.

- [x] **io_uring writer thread.** `--io-uring` on merge. O_DIRECT + WriteFixed with
  64 registered 256KB buffers (16MB). Feature-gated `linux-io-uring`.

  **Future optimizations:**
  - SQ polling (`setup_sqpoll`) — eliminates `io_uring_enter` syscalls, consumes a CPU core
  - `ReadFixed` + linked `WriteFixed` for CopyRange — avoids userspace read buffer
  - `pread` directly into registered buffer instead of heap allocation

## Dependencies

- [ ] CLI-only dependencies (`clap`, `quick-xml`, `serde_json`) are runtime deps of the library
  crate. Library-only users pay the compile cost. Consider a `cli` feature gate or separate
  `pbfhogg-cli` binary crate.
- [x] ~~`protobuf` crate~~ — migrated to `prost` v0.14 + `protox` v0.9.

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

- [ ] `src/indexed.rs:42` — `relation_ids` field in `IdRanges` is populated but
  unused. `IndexedReader` only has `read_ways_and_deps` (2-pass: filter ways →
  fetch dependent nodes) and `for_each_node`. A `read_relations_and_deps` would
  need 3+ passes: pass 1 filter relations → collect member way/node/relation IDs;
  pass 2 fetch member ways → collect their node refs; pass 3 fetch all dependent
  nodes. Recursive relation members (relations containing relations) add another
  pass or fixpoint loop. The `relations_available()` method is already written
  but commented out (line 80-89). The field and method are zero-cost as-is —
  park until a concrete consumer exists (e.g. extract --smart, or a library user
  doing relation-based filtering).

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
