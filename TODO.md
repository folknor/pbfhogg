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

Most users won't use io_uring or erofs. The generic buffered I/O path needs to
be excellent on its own. Read throughput is already strong (0.31s parallel, 1.3s
pipelined on Denmark). The gaps are in CLI commands and the buffered merge path.

- [ ] **CLI command performance vs osmium.** Current numbers on Denmark 465 MB
  (commit `3944a3f`, plantasjen, solo runs via `verify/*.sh`):

  | Command | pbfhogg | osmium | ratio | notes |
  |---------|---------|--------|-------|-------|
  | cat --type way | **1.06s** | 2.23s | **0.48x** | parallel + blob-skip (indexdata) |
  | tags-filter amenity=restaurant -R | **0.46s** | 1.16s | **0.40x** | parallel + blob-skip |
  | getid (9 elements) | **0.38s** | 0.84s | **0.45x** | parallel |
  | tags-count --type way | **0.35s** | 0.60s | **0.58x** | fold+reduce + blob-skip (indexdata) |
  | tags-filter w/highway=primary -R | **0.44s** | 0.55s | **0.80x** | parallel + blob-skip |
  | tags-filter highway=primary 2pass | 2.69s | 2.42s | 1.11x | two-pass, parallel Pass 2 |
  | add-locations-to-ways | 11.42s | 11.98s | 0.95x | Pass 1 hash build is bottleneck |
  | extract --simple | **2.48s** | 1.69s | 1.47x | skip-metadata + Pass 2 parallel |
  | extract (complete-ways) | **2.48s** | 2.79s | **0.89x** | skip-metadata + Pass 2 parallel |
  | extract --smart | **2.83s** | 3.25s | **0.87x** | skip-metadata + ways-only Pass 2 |

  All commands use pipelined reader + pipelined writer. All write passes use
  parallel element processing via rayon batches (64 blocks per dispatch).
  Blob-type skipping via indexdata provides additional gains for type-filtered
  commands. Ratios below 1.0 = pbfhogg is faster. Numbers from
  `scripts/bench-commands.sh` on Denmark 483 MB, commit `1b62e2c`.

  Japan extract (2.3 GB, 344M elements, Tokyo bbox, commit `1b62e2c`):

  | Strategy | pbfhogg | osmium | ratio |
  |----------|---------|--------|-------|
  | simple | 12.2s | **7.2s** | 1.70x |
  | complete-ways | 12.8s | **11.0s** | 1.16x |
  | smart | 14.6s | **13.4s** | 1.09x |

  Remaining:

  - [ ] **Extract simple: remaining gap vs osmium.** Simple is 1.47x slower
    on Denmark, 1.70x on Japan. **Complete-ways and smart now beat osmium.**
    Full analysis in `notes/extract-parallel-collection.md`.

    The gap is structural: simple does 2 passes vs osmium's 1. The extra file
    read costs ~1.3s on Denmark, ~5s on Japan. A single-pass approach with
    parallel inline writing would close most of this gap.

    **Implemented optimizations:**
    - ~~Skip-metadata mode for scan-only passes~~ — `elements_skip_metadata()`
      skips 6 packed metadata arrays per dense node (version, timestamp,
      changeset, uid, user_sid, visible). Denmark: -7% simple, -10% complete,
      -34% smart. Japan: -16% simple, -10% complete, -39% smart.
    - ~~Smart Pass 2 ways-only iteration~~ — `block.groups()` / `group.ways()`
      skips all dense nodes, sparse nodes, and relations in the way dependency
      resolution pass. This was the main contributor to the 34-39% smart gain.

    **Investigated and ruled out:**
    - ~~Parallel ID collection with IndexedReader~~ — implemented three-phase
      parallel collection (blob buffering + rayon `try_fold`/`try_reduce`),
      benchmarked on Denmark (483 MB) and Japan (2.3 GB). No improvement: the
      pipelined reader already parallelizes decompression, and the collection
      consumer work (bbox check, ID set insert) is ~5% of per-block time.
      Buffering all compressed blobs adds I/O + allocation overhead that
      negates any parallelism gains.
    - ~~Concurrent ID sets (dashmap/atomic bitsets)~~ — not applicable since
      the bottleneck is decode, not consumer writes.
    - ~~io_uring reads~~ — merge benchmarks show io_uring only helps above
      ~15 GB (beyond page cache). Extract reads are sequential scans where
      kernel readahead with `posix_fadvise` is already optimal.

    **Possible approach:**
    - [ ] **Single-pass simple with parallel inline writing.** Stream through
      the pipelined reader, collect + filter matching elements per block, batch
      matched blocks for parallel writing via rayon. Eliminates the second file
      read. Challenge: the collection consumer (which is sequential) and the
      write dispatch (which needs rayon) must coexist in the same pass.

    **Remaining bottleneck: `add-locations-to-ways` Pass 1 (hash index building).**
    Pass 2 is now parallel, but Pass 1 (building the FxHashMap node index) is
    sequential and dominates wall time. Denmark: ~11.3s total vs osmium ~10.3s.
    The hash index build is ~6-7s (52.5M node inserts into FxHashMap). Options:
    - Parallel hash map build: partition nodes by ID range across threads, each
      builds a sub-map, then merge. Or use a concurrent map (dashmap/flurry).
    - The Dense mmap index variant avoids this entirely (direct indexing, no
      hash table) but requires `vm.overcommit_memory=1` for planet-scale capacity.

- [ ] **Zstd compressor state reuse.** The `zstd` crate has the same per-call
  encoder problem (`zstd::stream::write::Encoder::new()` allocates fresh state
  each blob). `zstd::bulk::Compressor` provides a reusable whole-buffer
  compressor with `compress_to_buffer()`. Same pattern as the zlib fix — add to
  FrameScratch, reuse across blobs. Lower priority since zstd PBFs are rare in
  the OSM ecosystem.

- [ ] **Buffered merge at planet scale.** North America buffered merge is 43s (zlib)
  / 36s (none) vs io_uring's 33s/25s. The buffered path could be improved with
  read-ahead for passthrough blobs and reduced syscall overhead without requiring
  io_uring.

- [ ] **Large folios for mmap reads.** On 6.14+, file-backed mmap gets transparent
  2MB huge pages automatically. Low priority — mmap is not the production hot path
  and is already the slowest read mode. Only relevant at planet scale (80GB, 20M
  TLB entries). If implemented, should be opt-in to avoid regressing small files.

## Before crates.io publish

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Verify edition 2024 is intentional — most published crates use 2021 for broader compatibility
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

## Nice to have

- [ ] Consider adding `serde` feature for element serialization
