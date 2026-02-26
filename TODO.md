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

## Performance: parallelism

- [ ] `pipeline.rs:14-18` — `READ_AHEAD=16` / `DECODE_AHEAD=32` are hardcoded.
  `READ_AHEAD` bounds the `sync_channel` between the I/O thread (Stage 1) and
  the rayon decode pool (Stage 2) — the I/O thread blocks when 16 compressed
  blobs are buffered. `DECODE_AHEAD` bounds the channel between the decode pool
  and the main-thread reorder buffer (Stage 3) — decode threads block when 32
  decoded blocks are pending. `DECODE_AHEAD` is 2× `READ_AHEAD` because decode
  results arrive out-of-order and the reorder `VecDeque` needs headroom to
  reconstruct file order without stalling Stage 1.

  Backpressure is automatic via bounded `sync_channel`: if the main thread's
  `block_fn` is slow, the decode channel fills → decode threads block on send →
  raw channel fills → I/O thread blocks on send. No manual tuning needed.

  Memory cost: ~16 × 32KB (compressed) + 32 × 1.4MB (decoded) ≈ **51 MB** peak
  pipeline overhead, independent of file size. The `DecompressPool` recycles
  decode buffers so cumulative alloc is near-zero (vs 10.2 GB naive for Denmark,
  ~1.7 TB for planet).

  Making these configurable would require a pipeline config struct on the public
  `for_each_pipelined` API. Hotpath profiling (Denmark through Japan) shows the
  pipeline is balanced at all tested scales — I/O thread doesn't stall, rayon
  workers are barely loaded, main thread is the bottleneck. **Low priority** —
  configure when someone reports a problem on a memory-constrained system.

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

  **Current rayon usage (3 sites, all working well):**

  | Site | Pattern | Pool | Purpose |
  |------|---------|------|---------|
  | `pipeline.rs:85-104` | `ThreadPoolBuilder` + `spawn` | Dedicated | Decode pool (Stage 2) |
  | `writer.rs:289` | `rayon::spawn()` | Global | Parallel compression |
  | `merge.rs:1045` | `par_iter().map_init()` | Global | Batch classify |
  | `reader.rs:350` | `into_par_iter().try_fold().try_reduce()` | Global | `par_map_reduce` |

  The pipeline decode pool uses a dedicated `ThreadPoolBuilder` with `available_parallelism() - 2`
  threads (reserving 2 for I/O + consumer) and raw `rayon::spawn` — it doesn't use par_iter at
  all. The writer uses global-pool `rayon::spawn` for parallel compression. `par_map_reduce` batch-
  collects all blobs then uses lock-free `into_par_iter` (replaced an earlier `par_bridge` +
  Mutex approach that had contention at 8+ cores). Merge uses `par_iter().map_init(Vec::new, ...)`
  for per-thread decompression buffer reuse during classify.

  The `thread_local::ThreadLocal` trick could replace merge's `map_init(Vec::new, ...)`, but the
  practical gain is zero — `Vec::new()` is stack-only (no heap allocation until first push), so
  rayon re-initing it under work-stealing costs nothing. Switching to paralight would add a
  dependency for marginal benefit on a path that already works well. **Low priority** — revisit
  only if rayon becomes a proven bottleneck (e.g. if parallel `rewrite_block` exposes contention
  in the global pool).

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

- [x] **SQ polling (`setup_sqpoll`)** — implemented (`--sqpoll` flag), but crashes
  mid-stream on North America (18.8 GB) after ~4000 blobs. Needs debugging.
  Works at Denmark scale. Likely a CQE reaping timing issue with the kernel
  polling thread.

- [x] **`ReadFixed` + linked `WriteFixed` for CopyRange** — implemented. Linked
  SQE pairs eliminate pread syscalls for passthrough blobs. North America results:
  **-32%** (zlib), **-42%** (none) vs buffered. No improvement at Denmark/Japan
  scale (page cache absorbs everything).

- [x] **`pread` directly into registered buffer** — implemented as part of the
  linked ReadFixed+WriteFixed chain. No separate pread path needed.

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

## Benchmarking

- [ ] Track peak RSS during reads and merges at scale. Denmark for CI, planet for release validation.
- [ ] Run Germany full profiling suite (4.5 GB, ~500M elements). Currently only
  merge timing exists — missing read baselines (tags-count, check-refs),
  decode+write (cat --type), and allocations. Run:
  `scripts/profile-region.sh germany data/germany-20260224-seq4704.osm.pbf data/germany-20260225-seq4705.osc.gz`
  Then update `notes/region-profiles.md` with the results.

## CLI: verify all commands use pipelined writer

- [ ] Audit every CLI command that writes PBF output to verify it uses `to_path_pipelined`
  (not `to_path` sync). `cat` was observed running single-threaded on a 17.4 GB file
  (Threads: 1, ~11 MB RSS) — suggesting it uses the sync writer path. At planet scale,
  single-threaded zlib compression is the bottleneck. Commands to check:
  - `cat` — confirmed single-threaded, needs pipelined writer
  - `sort`
  - `extract`
  - `add-locations-to-ways`
  - `tags-filter`
  - `getid` / `removeid`

## Nice to have

- [ ] Consider adding `serde` feature for element serialization
