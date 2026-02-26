# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    scripts/test.sh -- --ignored

`sorted_flag_but_unsorted_nodes_panics` in `tests/read_paths.rs` is `#[ignore]` — it
verifies the debug monotonicity assertion fires on unsorted nodes when `Sort.Type_then_ID`
is declared. Requires `debug_assertions` to be enabled in the test profile. Nightly 1.95
(2026-02-25) has a regression where `debug_assertions` is off in test builds.

## Performance: hotpath profiling

Raw data and analysis in `notes/hotpath-profile.md`.
Run with `scripts/run-hotpath.sh` (timing) or `scripts/run-hotpath-alloc.sh` (timing + alloc).

### Investigations

- [x] **Benchmark pipelined writer with Compression::None** — the sync write
  benchmark (cat --type) shows frame_blob at 57% of wall time (zlib:6). The
  pipelined writer parallelizes compression across rayon workers. With
  Compression::None (nidhogg's production config on erofs), compression is
  eliminated entirely.

  **Results** (Denmark 483MB, ~59M elements, best of 3, commit 3383873,
  `examples/bench_write.rs` writing to `/dev/null`):

  | Mode              | Time (ms) | Elem/s |
  |-------------------|-----------|--------|
  | write-none (sync) |      9646 |  6.1M  |
  | write-pipe-none   |      9627 |  6.1M  |
  | write-zlib:6 (sync) |  18916 |  3.1M  |
  | write-pipe-zlib:6 |      9223 |  6.4M  |

  **Analysis:**
  - Pipelined zlib:6 is the big win: 2.05× faster than sync zlib:6 (9.2s vs
    18.9s). Parallel compression on rayon workers fully hides the zlib cost.
  - With Compression::None, pipelined adds zero benefit (9627 vs 9646 ms).
    There is no compression work to overlap — the pipeline has nothing to
    parallelize.
  - The write throughput floor is ~9.2–9.6s for Denmark. Both write-pipe-none
    and write-pipe-zlib:6 converge there, confirming the bottleneck is the
    single-threaded decode → BlockBuilder → protobuf serialize loop, not I/O
    or compression.
  - Pipelined zlib:6 is marginally faster than pipelined none (9223 vs 9627).
    With `/dev/null` as output, I/O is free either way; compressed blocks are
    smaller, meaning less memory traffic and smaller `write()` syscall payloads.

  **Implication for nidhogg:** on erofs with Compression::None, the pipelined
  writer provides no throughput advantage over sync. The optimization target
  for nidhogg's write path is the sequential decode + BlockBuilder + serialize
  loop (~9.2s at Denmark scale). Further write speedup requires either
  parallelizing the decode/serialize work itself (multi-threaded BlockBuilder)
  or reducing per-element serialization cost (raw passthrough, cheaper protobuf
  encoding).

- [x] **Re-generate test PBF through pbfhogg for indexdata** — re-generated
  denmark-seq4704 through `cat --type node,way,relation` producing
  `data/denmark-20260220-seq4704-with-indexdata.osm.pbf` (465 MB, indexdata
  in all 7396 blobs). Hotpath script updated to use this file.

  **Results:** classify_blob 3.26s → 609ms (5.4×). With indexdata + zlib,
  wall time is 5.16s (compression-bound). With indexdata + Compression::None
  (nidhogg production path), wall time is **1.90s** — 1.84× faster than
  the old 3.50s baseline. New bottleneck is rewrite_block at 49%.
  Full data: `notes/hotpath-profile.md`.

- [x] **`block_builder::take` buffer reuse** — take allocated 4.6 GB total
  from `encode_to_vec()` creating a fresh buffer every flush. Fixed by
  storing a `Vec<u8>` in BlockBuilder and using `encode(&mut buf)` +
  `buf.clear()`. Return type changed from `Vec<u8>` to `&[u8]`.
  Eliminates ~960 MB encode churn per Denmark run.
  Investigation: `notes/take-buffer-reuse.md`.

### Merge: remaining optimization theories

With indexdata + Compression::None (nidhogg production path), merge takes
1.90s on Denmark. The bottleneck is `rewrite_block` (49%) + `classify_blob`
(32%) + `block_builder::take` (31%) — the decode/re-encode work on the ~630
rewritten blocks. These process ~4.4M elements at Denmark scale.

**Germany scale (4.5 GB, measured):** Rewrite fraction jumps to 18.4%
(11,480 / 62,461 blobs). With indexdata + Compression::None, wall time is
**52.3s** — paradoxically *slower* than indexdata + zlib (49.9s). Without
parallel compression work, there's nothing to overlap main-thread serial
work with. The zlib path hides ~30s of main-thread work behind 110s of
parallel compression. **Compression::None only wins when rewrite_block is
also parallelized.** Thread utilization confirms: zlib workers 83-92% busy,
none workers idle.

**Planet-scale extrapolation (75 GB):** Rewrite fraction rises to ~92%.
Merge degrades to near-full-rewrite performance: ~27 min for single-threaded
`rewrite_block` (~1.1M blobs × 1.49ms each). The per-blob micro-
optimizations below each save <10% of rewrite_block's per-call cost — at
planet scale they shave ~1-3 min off a 27-min bottleneck. The structural
optimization is **parallelizing rewrite_block itself**, which also unlocks
Compression::None as the faster path. Full analysis: `notes/hotpath-profile.md`.

- [x] **Element-level raw passthrough in rewrite_block** — investigated, not
  feasible. String table index coupling (all types) and cross-element delta
  encoding (dense nodes) make per-element raw byte splicing impossible without
  re-serialization that costs the same as full reencode.

- [x] **StringTable::add allocation-free fast path** — `add()` called
  `entry(s.to_owned())` on every invocation, allocating a String even on cache
  hits (~99% of calls). Fixed by trying `self.index.get(s)` first (zero-alloc
  Borrow trait lookup), falling through to `entry()` only on miss. Results:
  add_node 55→41ns (25%), add_way 255→219ns (14%), add_relation 691→491ns (29%),
  merge rewrite_block 2.17→2.03s (6.5%). Investigation: `notes/rewrite-block-cost-breakdown.md`.

- [x] **Pre-seed output StringTable / index passthrough for merge** — at block
  start, pre-seed the output StringTable from the input block (identity mapping).
  Base elements (~99.9%) use `raw_tags()` + `add_*_raw()` methods — no hash, no
  probe, no string decode. Diff elements (~0.1%) use normal `add(&str)`. A
  `pre_seeded` flag on BlockBuilder tracks validity across mid-block flushes
  (emit_before can flush + add diff elements, invalidating the pre-seed; the
  next `write_base_*` detects this via `is_pre_seeded()`, flushes the
  non-pre-seeded content, and re-seeds). Investigation: `notes/preseed-stringtable.md`.

- [ ] **Raw packed bytes for non-string integer fields** — investigated: the
  delta encoding is compatible (both input wire format and BlockBuilder delta-
  encode refs/memids from 0 within each element), so raw byte passthrough is
  valid. Previously blocked by prost's `Vec<i64>` types requiring decode+reencode.
  **Direct wire encoding (see BlockBuilder section) removes this blocker** — with
  manual protobuf emission, `add_way_raw` can accept raw packed bytes for refs
  and write them directly to `packed_scratch` without decoding. Same for relation
  memids/roles_sid/types. Bottom-up estimate: ~74ms (3.7% of rewrite_block) —
  small but essentially free once direct encoding is in place.
  Detailed cost analysis: `notes/rewrite-block-cost-breakdown.md`.

- [x] **Protobuf serialization in `take`** — re-benchmarked with prost: 739ms
  (slightly slower than old crate's 673ms). Buffer reuse now implemented (see
  above) — `encode_to_vec()` replaced with `encode(&mut buf)` + reused buffer.

### Merge: passthrough blob I/O optimization

Investigation of the uninstrumented time gap in merge (Japan: 10s instrumented,
24.4s wall = 14s gap; Norway: 1.5s instrumented, 9.1s wall = 7.6s gap).

**Root cause:** Every passthrough blob gets **4 copies** in userspace:
1. Disk → `blob_bytes` Vec (~55 KB) — `read_raw_frame` reads blob data
2. `blob_bytes` → `frame_bytes` Vec (~55 KB) — assembled in `read_raw_frame`
3. `frame_bytes` → `.to_vec()` (~55 KB) — `write_raw` copies for writer channel
4. Channel Vec → BufWriter → disk

Copies 2 and 3 are unnecessary. For Japan (43K blobs), `read_raw_frame` allocates
~4.5 GB (2.3 GB `blob_bytes` + 2.2 GB `frame_bytes`), and `write_raw` adds ~2.1 GB
from `.to_vec()`. The `batch.clear()` / alloc cycle (64 frames per batch, dropped
and re-allocated every iteration) adds further overhead.

- [ ] **Eliminate `frame_bytes` duplication in `RawBlobFrame`** — store only one
  buffer, derive `blob_bytes` as a slice into `frame_bytes[4+header_len..]`.
  Eliminates one ~55 KB alloc per blob. Estimated: -2.3 GB alloc, -1-2s for Japan.

- [ ] **`write_raw_owned()` — move Vec into channel instead of `.to_vec()`** — add
  a `write_raw_owned(Vec<u8>)` method to `PbfWriter` that sends the Vec directly
  to the writer thread channel without copying. Merge passthrough path uses
  `std::mem::take(&mut frame.frame_bytes)` to move ownership. Estimated: -2.1 GB
  alloc, -1-2s for Japan.

- [ ] ~~**Buffer pool for `RawBlobFrame` across batches**~~ — **conflicts with
  `write_raw_owned`**: once the Vec is moved into the writer channel, the pool never
  gets it back. The pool only helps the 2-8% rewritten blobs (where `frame_bytes` is
  not consumed). A backward channel (writer returns buffers via Mutex) adds cross-
  thread sync for marginal gain. Mmap zero-copy also rejected: `copy_file_range`
  already handles zero-copy on Linux, and mmap is incompatible with O_DIRECT.
  **Recommendation: drop this, optimizations 1+2 are sufficient** (~4.4 GB saved,
  67% reduction). The remaining ~2.2 GB (one `frame_bytes` alloc per blob) is a
  tight alloc/free pattern that modern allocators handle efficiently.

- [ ] **Avoid `Bytes::copy_from_slice` in `decompress_blob_data_into`** —
  `classify_blob` calls `decompress_blob_data_into(&frame.blob_bytes, buf)` which
  internally does `Bytes::copy_from_slice` on the entire compressed blob just to
  parse the Blob protobuf envelope. Since `prost::Message::decode` accepts
  `impl Buf` and `&[u8]` implements `Buf`, the copy is unnecessary.

- [ ] **Avoid `Bytes::copy_from_slice` in `parse_blob_header_with_index`** —
  `blob.rs:733` copies header bytes (~50 bytes) into a `Bytes` just to call prost
  decode. Same fix: decode directly from `&[u8]`. Tiny per-blob but 43K unnecessary
  copies for Japan.

**Combined impact (optimizations 1+2 only):** Save ~4.4 GB from `read_raw_frame`
+ ~2.1 GB from `write_raw` (67% passthrough alloc reduction). Uninstrumented gap
shrinks from ~14s to ~5-7s (actual disk I/O floor) for Japan. For Norway, gap
shrinks from ~7.6s to ~3-4s. Only merge.rs needs changes — sort.rs and cat.rs
use sync mode (no `.to_vec()` copy), so `write_raw_owned` is not needed there.
Cat.rs has the same `RawBlobFrame` duplication (optimization 1 applicable for
consistency, but not urgent).

### BlockBuilder: direct wire-format encoding for ways/relations

Investigation of per-element allocation costs in `add_way`. Cross-region data:
Japan 588 bytes/call (42.9M ways, 25.2 GB total), Norway 900 bytes/call
(coastline refs), London 325ns/call (highest time, dense urban tagging).

**Root cause:** `add_way` creates a fresh `proto::Way` with 5 zero-capacity Vecs
per call, grows each from zero through multiple doublings. Contrast: `add_node`
pushes into pre-allocated reused arrays (explains 21ns vs 205ns — 10× difference).

Per-call allocation breakdown (typical way, 6 tags, 8 refs):
- `way.refs` Vec growth (0→1→2→...→next_pow2): ~120 bytes alloc traffic (biggest)
- `way.keys`/`way.vals` Vec growth: ~48 bytes each
- `self.ways` Vec growth (amortized): ~335 bytes/call in traffic
- Norway coastlines (100+ refs): pushes to 900 bytes/call

**Decision: direct wire-format encoding.** The read path already bypasses prost
with ~900 lines of custom wire-format code in `src/read/wire.rs` (Cursor,
PackedIter, varint decode, zigzag encode/decode). Direct encoding on the write
side is the natural complement. It subsumes all incremental prost-based fixes
(pre-allocate Vecs, column-oriented storage, way.lat/lon waste, add_way_raw
growth) and also unlocks raw packed bytes passthrough for merge.

- [x] **Direct protobuf serialization (bypass `proto::Way` entirely)** — instead
  of building `proto::Way` objects and encoding them via prost, accumulate raw
  protobuf bytes directly during `add_way`. Eliminates nearly all per-call
  allocation (~580 bytes/call saved) and prost's two-pass encode (encoded_len +
  encode_raw). ~+315 net lines. **Result: pipelined write floor 9.0s → 7.0s
  (22% faster). Sync none 9.0→7.1s, zstd 11.0→9.1s, zlib 17.5→15.5s.**

  **Design (investigated):** Replace `ways: Vec<proto::Way>` and `relations:
  Vec<proto::Relation>` with 4 reusable `Vec<u8>` scratch buffers:
  - `group_buf` — per-block accumulator for all serialized way/relation submessages
  - `elem_scratch` — per-element body (cleared per call, capacity reused)
  - `packed_scratch` — per-field packed content (keys, vals, refs as varints)
  - `info_scratch` — Info sub-message body

  Encoding flow per `add_way`: encode field tags + varints into `elem_scratch`,
  packed fields go through `packed_scratch` (encode content → measure length →
  write tag+length+content to `elem_scratch`), then wrap with PrimitiveGroup
  field 3 tag + length prefix → append to `group_buf`. String table indices
  collected via `string_table.add()` as today. `group_buf.clear()` in `reset()`
  keeps capacity for next block (unlike old `mem::take` which zeroed it).

  `take()` branches: dense nodes stay on prost (already optimized), ways/relations
  use manual encoding — `StringTable::encode_to()` + `group_buf` concatenation
  into `encode_buf`. Borrow-checker handled by extracting `encode_way` /
  `encode_relation` as free functions taking individual `&mut` field refs.

  Covers all variants: `add_way` (string tags via string_table.add), `add_way_raw`
  (raw u32 indices, no string_table.add), `add_way_with_locations` (fields 9/10
  lat/lon), `add_relation` and `add_relation_raw` (fields 8/9/10 = roles_sid/
  memids/types). All field tags ≤ 15, so single-byte tag encoding. Negative
  int32 values sign-extend to 10-byte varints (matches prost). Output must be
  bit-identical to current prost encoding — verified by roundtrip tests + verify
  scripts. Design details: `notes/direct-wire-encoding.md` (to be written).

- [x] **StringTable `clear()` instead of `replace()`** — implemented as part of
  direct wire encoding. `StringTable::clear()` reuses existing Vec/HashMap
  allocations. Called in `reset()` for all block types.

**Superseded items** (all addressed by direct wire encoding):
- ~~Pre-allocate refs/keys/vals from known lengths~~ — no proto::Way Vecs to reserve.
- ~~Pre-allocate `self.ways` Vec to 8000~~ — replaced by `group_buf` with `clear()`.
- ~~Column-oriented way storage~~ — direct encoding is strictly more powerful.
- ~~`add_way_raw` Vec growth~~ — covered by `encode_way_raw` free function.
- ~~`way.lat`/`way.lon` unused~~ — no `proto::Way` created at all.

## Performance: parallelism

- [ ] **Parallel merge rewrite_block** — at planet scale with daily diffs,
  `rewrite_block` is single-threaded and takes ~27 min (~1.1M blobs × 1.49ms
  each). This is the dominant merge bottleneck. The current merge loop is
  sequential: read batch → parallel classify → sequential rewrite/passthrough.
  A pipelined merge architecture could parallelize the rewrite work:
  - **Stage 1** (I/O thread): `read_raw_frame` — sequential disk reads
  - **Stage 2** (rayon pool): `classify_blob` + `rewrite_block` — decompress,
    classify, and rewrite overlapping blocks in parallel
  - **Stage 3** (writer thread): ordered output via reorder buffer (like the
    existing pipelined writer)

  Challenge: `CreateEmitter` interleaves new elements between blocks at their
  sorted position. This requires cross-block coordination — a block's rewrite
  depends on knowing which creates go before it. Possible approaches:
  - Two-pass: first pass determines create insertion points, second pass
    rewrites blocks in parallel with known create boundaries
  - Batch-level: parallelize within batches (current 64-blob batches), keep
    cross-batch ordering sequential
  - Approximate: rewrite blocks in parallel, append creates at block boundaries
    (relaxing strict sorted order within block gaps — OSM consumers tolerate
    this)

  At 8 cores, parallel rewrite could reduce planet merge from ~27 min to
  ~3-4 min. Combined with indexdata + Compression::None, this would make
  daily planet refresh a ~5-min operation.

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

## Performance: Linux kernel features for planet-scale I/O

Research notes: `notes/linux-async-io.md`.

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

## Library API: PrimitiveBlock ergonomics

- [x] **`PrimitiveBlock::block_type()`** — public `BlockType` enum
  (`DenseNodes`, `Nodes`, `Ways`, `Relations`, `Mixed`, `Empty`) with
  convenience methods (`is_nodes()`, `is_ways()`, `is_relations()`).
  Classification reads one byte per group (first wire tag) — zero element
  decoding. Useful for consumers of `for_each_block_pipelined` /
  `into_blocks_pipelined` that route blocks by type.

- [x] **Sorted monotonicity assertion for block-level APIs** — resolved
  by documenting the gap. `for_each_block_pipelined` and
  `into_blocks_pipelined` rustdoc now notes that the debug assertion is
  not applied at this level, directing users to `for_each_pipelined` or
  manual checking. Moving the assertion into `run_pipeline` was rejected:
  it would add latency to the critical path for block-level consumers
  (who route blocks to other threads), and couples the transport layer
  to header-level PBF semantics.

## Library API: Sort.Type_then_ID ergonomics

**Read side (done):** `ElementReader` now parses the PBF header eagerly at
construction. `reader.header().is_sorted()` tells callers whether the PBF
declares `Sort.Type_then_ID`. In debug builds, `for_each` and
`for_each_pipelined` assert that node IDs arrive in ascending order when
the flag is set.

**Write side (done):** `HeaderBuilder` replaces the old `build_header()`
function with a type-safe builder pattern. `.sorted()` sets the flag without
string manipulation. `HeaderBuilder::from_header(&header)` copies bbox and
replication metadata from an existing header. The builder's rustdoc includes
a usage example showing sorted writes.

- [x] ~~Consider a `PbfWriter::write_sorted_header()` convenience method~~
  Replaced by `HeaderBuilder::new().sorted().build()` — builder pattern is
  more general than a single convenience method.
- [x] ~~Document the Sort.Type_then_ID requirement in `build_header()` rustdoc~~
  `HeaderBuilder` rustdoc documents the `.sorted()` method and includes examples.
- [x] ~~Add a library-level example showing sorted write with the feature flag~~
  `HeaderBuilder` doc example shows `HeaderBuilder::new().bbox(...).sorted().build()`.

## Before crates.io publish

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Verify edition 2024 is intentional — most published crates use 2021 for broader compatibility
- [ ] Add `tests/test.osm.pbf` to version control (generated by `cargo run --example gen_test_pbf`)
- [x] ~~Make writing program configurable in `build_header()` instead of hardcoded "pbfhogg"~~
  `HeaderBuilder::new().writing_program("my-tool")` overrides the default.
- [ ] Fix crate-level doc example: says `pbfhogg = "0.1"` but Cargo.toml is 0.2.0
- [ ] Add doc comments to `writer.rs` public API (PbfWriter, Compression)
- [ ] Add doc comments to `block_builder.rs` public API (BlockBuilder, Metadata, MemberData)
- [ ] Add crate-level documentation for write/merge workflows (lib.rs)
- [ ] Tighten module visibility: `pub mod commands`, `pub mod osc`, `pub use
  read::file_reader`, `pub use write::file_writer` expose internals as public API
- [ ] Fix `error.rs:27` doc: says "when reading PBF files" but errors occur during writing too
- [ ] Publish to crates.io

## GitHub

- [ ] Write GitHub repo description and tags (openstreetmap, pbf, protobuf, osm, rust)
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

## Refactoring: duplicated metadata extraction

`dense_node_metadata()`, `element_metadata()`, `dense_node_raw_metadata()`,
`element_raw_metadata()`, `flush_block()`, and `rebuild_header()` are shared
helpers in `commands/mod.rs`.

sort.rs still has its own inline metadata extraction (uses `OwnedMetadata`
with owned `String` instead of borrowed `Metadata<'a>`).

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
(version-based vs unconditional). See git log and `verify/merge.sh` for details.

## Benchmarking

- [ ] Track peak RSS during reads and merges at scale. Denmark for CI, planet for release validation.
- [ ] Run Germany full profiling suite (4.5 GB, ~500M elements). Currently only
  merge timing exists — missing read baselines (tags-count, check-refs),
  decode+write (cat --type), and allocations. Run:
  `scripts/profile-region.sh germany data/germany-20260224-seq4704.osm.pbf data/germany-20260225-seq4705.osc.gz`
  Then update `notes/region-profiles.md` with the results.

## Nice to have

- [ ] Consider adding `serde` feature for element serialization
