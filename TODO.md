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

## Performance: merge passthrough I/O

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

- [ ] **SQ polling (`setup_sqpoll`)** — eliminates `io_uring_enter` syscalls,
  consumes a CPU core. Follow-up to the existing io_uring writer thread.

- [ ] **`ReadFixed` + linked `WriteFixed` for CopyRange** — avoids userspace read
  buffer for passthrough blobs. Follow-up to the existing io_uring writer thread.

- [ ] **`pread` directly into registered buffer** — instead of heap allocation.
  Follow-up to the existing io_uring writer thread.

## Before crates.io publish

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Verify edition 2024 is intentional — most published crates use 2021 for broader compatibility
- [ ] Add `tests/test.osm.pbf` to version control (generated by `cargo run --example gen_test_pbf`)
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
